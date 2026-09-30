use chrono::Local;
use rusqlite::{params, Connection, DatabaseName, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use tauri::{
    menu::{Menu, MenuItemBuilder, MenuItemKind, SubmenuBuilder},
    Manager,
};
#[cfg(target_os = "macos")]
use tauri::menu::{AboutMetadata, PredefinedMenuItem};
use walkdir::WalkDir;

mod compatibility;
use compatibility::*;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Project {
    id: String,
    name: String,
    roots: Vec<String>,
    session_count: usize,
    issue_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    id: String,
    title: String,
    project_id: Option<String>,
    project_name: Option<String>,
    project_roots: Vec<String>,
    database_cwd: String,
    conversation_cwd: Option<String>,
    log_path: Option<String>,
    archived: bool,
    status: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OrphanRecord {
    id: String,
    conversation_cwd: Option<String>,
    log_path: String,
    archived: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    projects: usize,
    sessions: usize,
    matched: usize,
    mismatched: usize,
    unlinked: usize,
    missing_logs: usize,
    orphan_records: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuditReport {
    codex_home: String,
    state_database: String,
    default_backup_directory: String,
    projects: Vec<Project>,
    sessions: Vec<Session>,
    orphans: Vec<OrphanRecord>,
    summary: Summary,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairRequest {
    thread_id: String,
    target_path: String,
    confirmation: String,
    backup_base: Option<String>,
    #[serde(default)]
    include_child_agents: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteRequest {
    log_path: String,
    confirmation: String,
    backup_base: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteSessionRequest {
    thread_id: String,
    confirmation: String,
    backup_base: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectRepairRequest {
    project_id: String,
    target_path: String,
    confirmation: String,
    backup_base: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectDeleteRequest {
    project_id: String,
    confirmation: String,
    backup_base: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RollbackRequest {
    manifest_path: String,
    backup_base: Option<String>,
    confirmation: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteRollbackRequest {
    manifest_path: String,
    backup_base: Option<String>,
    confirmation: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupDeleteRequest {
    backup_folder: String,
    backup_base: Option<String>,
    confirmation: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClearHistoryRequest {
    kind: String,
    backup_base: Option<String>,
    confirmation: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportSessionsRequest {
    thread_ids: Vec<String>,
    destination_directory: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportSessionsRequest {
    manifest_path: String,
    confirmation: String,
    #[serde(default)]
    project_mappings: Vec<ImportProjectMapping>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportProjectMapping {
    project_id: String,
    target_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportProject {
    id: String,
    name: String,
    roots: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportLog {
    thread_id: String,
    file: String,
    archived: bool,
    #[serde(default)]
    canonical: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionExportManifest {
    format: String,
    version: u32,
    exported_at: String,
    projects: Vec<ExportProject>,
    visible_thread_ids: Vec<String>,
    thread_ids: Vec<String>,
    #[serde(default)]
    project_assignments: HashMap<String, String>,
    logs: Vec<ExportLog>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history_database: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    thread_path_settings: HashMap<String, HashMap<String, Value>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportPackageInfo {
    manifest_path: String,
    exported_at: String,
    projects: Vec<ExportProject>,
    visible_thread_count: usize,
    thread_count: usize,
    log_count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionResult {
    message: String,
    backup_folder: String,
    changes: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessCloseResult {
    message: String,
    requested: usize,
    remaining: usize,
}

#[derive(Clone, Debug)]
struct ResolvedImportProject {
    source_project_id: String,
    app_server_project_id: String,
    desktop_project_id: String,
    target_path: String,
}

struct DesktopImportPlan {
    state_path: PathBuf,
    state: Value,
    projects: HashMap<String, ResolvedImportProject>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupFileEntry {
    original_path: String,
    backup_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RepairManifest {
    version: u32,
    created_at: String,
    thread_id: String,
    session_title: String,
    source_cwd: String,
    target_cwd: String,
    files: Vec<BackupFileEntry>,
    #[serde(default)]
    thread_changes: Vec<ThreadRepairChange>,
    rolled_back_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadRepairChange {
    thread_id: String,
    session_title: String,
    source_cwd: String,
    target_cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox_policy: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    desktop_path_settings: HashMap<String, Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RepairHistoryItem {
    created_at: String,
    thread_id: String,
    session_title: String,
    source_cwd: String,
    target_cwd: String,
    backup_folder: String,
    manifest_path: String,
    file_count: usize,
    rolled_back_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteManifest {
    version: u32,
    created_at: String,
    completed_at: Option<String>,
    deletion_kind: String,
    thread_id: Option<String>,
    session_title: String,
    source_path: Option<String>,
    child_count: usize,
    files: Vec<BackupFileEntry>,
    #[serde(default)]
    rolled_back_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteHistoryItem {
    created_at: String,
    deletion_kind: String,
    thread_id: Option<String>,
    session_title: String,
    source_path: Option<String>,
    child_count: usize,
    backup_folder: String,
    file_count: usize,
    rolled_back_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupHistoryItem {
    folder: String,
    name: String,
    operation: String,
    created_at: String,
    file_count: usize,
    total_size: u64,
}

struct ZoomLevel(Mutex<f64>);

const DEFAULT_ZOOM: f64 = 1.0;
const ZOOM_STEP: f64 = 0.1;
const MIN_ZOOM: f64 = 0.5;
const MAX_ZOOM: f64 = 3.0;

#[derive(Clone, Debug)]
struct DbThread {
    id: String,
    title: String,
    source: String,
    cwd: String,
    archived: bool,
    rollout_path: String,
    project_id: Option<String>,
}

#[derive(Clone, Debug)]
struct RepairTarget {
    id: String,
    title: String,
    cwd: String,
    rollout_path: String,
    logs: Vec<LogMeta>,
}

#[derive(Clone, Debug)]
struct LogMeta {
    id: String,
    cwd: Option<String>,
    path: PathBuf,
    archived: bool,
}

fn codex_home() -> Result<PathBuf, String> {
    let path = if let Some(configured) = std::env::var_os("CODEX_HOME") {
        PathBuf::from(configured)
    } else {
        platform_home_directory()
            .ok_or("无法读取用户主目录（HOME 或 USERPROFILE）")?
            .join(".codex")
    };
    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!("未找到 Codex 数据目录：{}", path.display()))
    }
}

fn platform_home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn state_database(home: &Path) -> Result<PathBuf, String> {
    for candidate in [home.join("state_5.sqlite"), home.join("sqlite/state_5.sqlite")] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err("未找到 state_5.sqlite；请确认 Codex Desktop 已至少启动过一次。".into())
}

fn desktop_catalog_database(home: &Path) -> PathBuf {
    home.join("sqlite").join("codex-dev.db")
}

fn display_path(path: &Path) -> String {
    display_path_for_platform(path.to_string_lossy().into_owned(), cfg!(windows))
}

fn display_path_text(path: &str) -> String {
    display_path_for_platform(path.to_string(), cfg!(windows))
}

fn display_path_for_platform(mut value: String, windows: bool) -> String {
    if windows {
        if let Some(path) = value.strip_prefix(r"\\?\UNC\") {
            value = format!(r"\\{path}");
        } else if let Some(path) = value.strip_prefix(r"\\?\") {
            value = path.to_string();
        }
    }
    value
}

fn normalized_for_platform(path: &str, windows: bool) -> String {
    let mut value = path.trim().replace('\\', "/");
    if windows {
        if let Some(path) = value.strip_prefix("//?/UNC/") {
            value = format!("//{path}");
        } else if let Some(path) = value.strip_prefix("//?/") {
            value = path.to_string();
        }
    }
    while value.len() > 1
        && value.ends_with('/')
        && !(windows && value.len() == 3 && value.as_bytes().get(1) == Some(&b':'))
    {
        value.pop();
    }
    if windows { value.to_lowercase() } else { value }
}

fn normalized(path: &str) -> String {
    normalized_for_platform(path, cfg!(windows))
}

fn is_same_path(left: &str, right: &str) -> bool {
    normalized(left) == normalized(right)
}

fn resolved_file_path(path: &Path) -> PathBuf {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut resolved) = fs::canonicalize(ancestor) {
            for name in suffix.into_iter().rev() { resolved.push(name); }
            return resolved;
        }
        let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) else { return path.to_path_buf() };
        suffix.push(name); ancestor = parent;
    }
}

fn same_file_path(left: &Path, right: &Path) -> bool {
    left == right || is_same_path(&display_path(&resolved_file_path(left)), &display_path(&resolved_file_path(right)))
}

fn is_managed_log(home: &Path, path: &Path) -> bool {
    if path.components().any(|part| matches!(part, std::path::Component::ParentDir)) { return false; }
    let resolved = resolved_file_path(path);
    let in_sessions = resolved.starts_with(resolved_file_path(&home.join("sessions")));
    let in_archived = resolved.starts_with(resolved_file_path(&home.join("archived_sessions")));
    (in_sessions || in_archived) && path.extension().is_some_and(|ext| ext == "jsonl")
}

fn query_projects(conn: &Connection) -> Result<Vec<Project>, String> {
    let mut roots_by_project: HashMap<String, Vec<String>> = HashMap::new();
    let mut root_stmt = conn
        .prepare("SELECT project_id, path FROM project_roots ORDER BY project_id, position")
        .map_err(|e| format!("无法读取 Codex 项目路径：{e}"))?;
    let root_rows = root_stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    for root in root_rows {
        let (id, path) = root.map_err(|e| e.to_string())?;
        roots_by_project.entry(id).or_default().push(path);
    }

    let mut stmt = conn
        .prepare("SELECT id, name FROM projects ORDER BY position, id")
        .map_err(|e| format!("无法读取 Codex 项目：{e}"))?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    let mut projects = Vec::new();
    for row in rows {
        let (id, name) = row.map_err(|e| e.to_string())?;
        projects.push(Project {
            roots: roots_by_project.remove(&id).unwrap_or_default(),
            id,
            name,
            session_count: 0,
            issue_count: 0,
        });
    }
    Ok(projects)
}

fn query_threads(conn: &Connection) -> Result<Vec<DbThread>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, COALESCE(NULLIF(TRIM(name), ''), title), source, cwd, archived, rollout_path, project_id
             FROM threads ORDER BY recency_at_ms DESC, id DESC",
        )
        .map_err(|e| format!("无法读取 Codex 会话：{e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DbThread {
                id: row.get(0)?,
                title: row.get(1)?,
                source: row.get(2)?,
                cwd: row.get(3)?,
                archived: row.get::<_, i64>(4)? != 0,
                rollout_path: row.get(5)?,
                project_id: row.get(6)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.map(|row| row.map_err(|e| e.to_string())).collect()
}

fn descendant_thread_ids(conn: &Connection, root_thread_id: &str) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    let mut seen = HashSet::from([root_thread_id.to_string()]);
    let mut pending = VecDeque::from([root_thread_id.to_string()]);
    let mut statement = conn
        .prepare("SELECT child_thread_id FROM thread_spawn_edges WHERE parent_thread_id = ?1 ORDER BY child_thread_id")
        .map_err(|e| format!("无法读取子代理关系：{e}"))?;

    while let Some(parent_id) = pending.pop_front() {
        let rows = statement
            .query_map(params![parent_id], |row| row.get::<_, String>(0))
            .map_err(|e| format!("无法读取子代理关系：{e}"))?;
        for row in rows {
            let child_id = row.map_err(|e| format!("无法读取子代理关系：{e}"))?;
            if seen.insert(child_id.clone()) {
                pending.push_back(child_id.clone());
                ids.push(child_id);
            }
        }
    }
    Ok(ids)
}

fn load_repair_targets(
    conn: &Connection,
    root_thread_id: &str,
    include_child_agents: bool,
    logs_by_id: &HashMap<String, Vec<LogMeta>>,
) -> Result<Vec<RepairTarget>, String> {
    let mut ids = vec![root_thread_id.to_string()];
    if include_child_agents {
        ids.extend(descendant_thread_ids(conn, root_thread_id)?);
    }

    let mut statement = conn
        .prepare(
            "SELECT id, cwd, COALESCE(NULLIF(TRIM(name), ''), title), rollout_path
             FROM threads WHERE id = ?1",
        )
        .map_err(|e| format!("无法读取目标会话：{e}"))?;
    let mut targets = Vec::new();
    for id in ids {
        let (id, cwd, title, rollout_path) = statement
            .query_row(params![id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?)))
            .map_err(|e| format!("未找到{}：{e}", if id == root_thread_id { "目标会话" } else { "子代理会话" }))?;
        let logs = logs_by_id.get(&id).cloned().unwrap_or_default();
        if logs.is_empty() {
            return Err(format!("找不到{}“{title}”的 JSONL 日志，已拒绝修改任何会话。", if id == root_thread_id { "目标会话" } else { "子代理会话" }));
        }
        if !logs.iter().any(|log| same_file_path(&log.path, Path::new(&rollout_path))) {
            return Err(format!("找不到会话“{title}”当前数据库引用的日志，已拒绝修改。"));
        }
        targets.push(RepairTarget { id, title, cwd, rollout_path, logs });
    }
    Ok(targets)
}

fn parse_log(path: &Path, archived: bool) -> Option<LogMeta> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(80).flatten() {
        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = value.get("payload")?;
        let id = payload
            .get("id")
            .or_else(|| payload.get("session_id"))
            .and_then(Value::as_str)?
            .to_string();
        let cwd = payload.get("cwd").and_then(Value::as_str).map(str::to_string);
        return Some(LogMeta { id, cwd, path: path.to_path_buf(), archived });
    }
    None
}

fn scan_logs(home: &Path) -> Vec<LogMeta> {
    let mut logs = Vec::new();
    for (folder, archived) in [(home.join("sessions"), false), (home.join("archived_sessions"), true)] {
        if !folder.is_dir() { continue; }
        for entry in WalkDir::new(folder).follow_links(false).into_iter().flatten() {
            let path = entry.path();
            if path.is_file() && is_managed_log(home, path) {
                if let Some(log) = parse_log(path, archived) { logs.push(log); }
            }
        }
    }
    logs
}

fn safe_package_file(package_root: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if path.is_absolute() || path.components().any(|part| !matches!(part, std::path::Component::Normal(_))) {
        return Err("导出包包含无效的文件路径。".into());
    }
    let resolved = package_root.join(path);
    if !resolved.starts_with(package_root) { return Err("导出包包含越界的文件路径。".into()); }
    Ok(resolved)
}

fn safe_imported_thread_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn read_export_manifest(path: &Path) -> Result<SessionExportManifest, String> {
    if path.file_name().and_then(|name| name.to_str()) != Some("manifest.json") {
        return Err("请选择导出包中的 manifest.json。".into());
    }
    let content = fs::read_to_string(path).map_err(|e| format!("无法读取导出包清单：{e}"))?;
    let manifest: SessionExportManifest = serde_json::from_str(&content)
        .map_err(|e| format!("导出包清单格式无效：{e}"))?;
    if manifest.format != "codex-session-manager-export" || !(1..=2).contains(&manifest.version) {
        return Err("这不是受支持的 Codex 会话导出包。".into());
    }
    if manifest.thread_ids.is_empty() || manifest.visible_thread_ids.is_empty() {
        return Err("导出包不包含可导入的会话。".into());
    }
    let ids = manifest.thread_ids.iter().collect::<HashSet<_>>();
    if ids.len() != manifest.thread_ids.len() || manifest.thread_ids.iter().any(|id| !safe_imported_thread_id(id)) {
        return Err("导出包包含无效或重复的会话 ID。".into());
    }
    if manifest.visible_thread_ids.iter().any(|id| !ids.contains(id)) {
        return Err("导出包中的主会话范围无效。".into());
    }
    Ok(manifest)
}

fn quote_sql_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn database_table_exists(conn: &Connection, schema: &str, table: &str) -> Result<bool, String> {
    conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {}.sqlite_master WHERE type = 'table' AND name = ?1)", quote_sql_identifier(schema)),
        params![table],
        |row| row.get(0),
    ).map_err(|e| format!("无法检查数据库表 {schema}.{table}：{e}"))
}

fn database_table_columns(conn: &Connection, schema: &str, table: &str) -> Result<Vec<String>, String> {
    let mut statement = conn.prepare(&format!(
        "PRAGMA {}.table_info({})", quote_sql_identifier(schema), quote_sql_identifier(table)
    )).map_err(|e| format!("无法检查数据库字段 {schema}.{table}：{e}"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("无法读取数据库字段 {schema}.{table}：{e}"))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| format!("无法读取数据库字段 {schema}.{table}：{e}"))
}

// Copy by column name: newer Codex versions can add nullable/defaulted fields
// and snapshots can store the same fields in a different order.
fn copy_database_rows(
    conn: &Connection,
    source_schema: &str,
    table: &str,
    row_filter: &str,
    ignore_conflicts: bool,
) -> Result<(), String> {
    copy_database_rows_into(conn, source_schema, "main", table, row_filter, ignore_conflicts)
}

fn copy_database_rows_into(
    conn: &Connection, source_schema: &str, target_schema: &str,
    table: &str, row_filter: &str, ignore_conflicts: bool,
) -> Result<(), String> {
    let source_columns = database_table_columns(conn, source_schema, table)?;
    let target_columns = database_table_columns(conn, target_schema, table)?;
    if source_columns.is_empty() || target_columns.is_empty() {
        return Err(format!("源数据库或当前数据库缺少 {table} 表。"));
    }
    // INSERT OR IGNORE must not silently discard rows when a newer schema adds
    // a required field that an older snapshot cannot supply.
    let mut required = conn.prepare("SELECT name FROM pragma_table_info(?1, ?2) WHERE \"notnull\" = 1 AND dflt_value IS NULL")
        .map_err(|e| e.to_string())?;
    let required_columns = required.query_map(params![table, target_schema], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    for column in required_columns {
        if !source_columns.contains(&column) {
            return Err(format!("导出包或备份缺少 {table}.{column} 必需字段，请使用兼容的 Codex 版本后重试。"));
        }
    }
    let columns = target_columns.iter().filter(|column| source_columns.contains(column))
        .map(|column| quote_sql_identifier(column)).collect::<Vec<_>>().join(", ");
    if columns.is_empty() { return Err(format!("源数据库与当前数据库的 {table} 表没有兼容字段。")); }
    let insert = if ignore_conflicts { "INSERT OR IGNORE" } else { "INSERT" };
    let table = quote_sql_identifier(table);
    conn.execute(&format!(
        "{insert} INTO {}.{table} ({columns}) SELECT {columns} FROM {}.{table} WHERE {row_filter}",
        quote_sql_identifier(target_schema), quote_sql_identifier(source_schema),
    ), []).map_err(|e| e.to_string())?;
    Ok(())
}

// Codex migration 55 renamed both the table and its type column. Detect each
// database independently so old exports/deletion backups remain importable.
const THREAD_ATTACHMENT_TABLES: [(&str, &str); 2] = [
    ("thread_attachments", "attachment_type"),
    ("thread_artifacts", "artifact_type"),
];

fn copy_thread_attachments(conn: &Connection, source_schema: &str, scope_table: &str) -> Result<(), String> {
    let mut target = None;
    for (table, type_column) in THREAD_ATTACHMENT_TABLES {
        if database_table_exists(conn, "main", table)? {
            target = Some((table, type_column));
            break;
        }
    }
    let row_filter = format!("thread_id IN (SELECT id FROM {})", quote_sql_identifier(scope_table));
    for (source_table, source_type_column) in THREAD_ATTACHMENT_TABLES {
        if !database_table_exists(conn, source_schema, source_table)? { continue; }
        let source = format!("{}.{}", quote_sql_identifier(source_schema), quote_sql_identifier(source_table));
        let Some((target_table, target_type_column)) = target else {
            let has_rows: bool = conn.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {source} WHERE {row_filter})"),
                [], |row| row.get(0),
            ).map_err(|e| e.to_string())?;
            if has_rows { return Err("当前 Codex 数据库不支持会话附件，请升级 Codex 后重试；操作已取消以免丢失附件。".into()); }
            continue;
        };
        conn.execute(&format!(
            "INSERT INTO main.{} (id, thread_id, {}, identity_key, payload, created_at)
             SELECT id, thread_id, {}, identity_key, payload, created_at FROM {source} WHERE {row_filter}",
            quote_sql_identifier(target_table), quote_sql_identifier(target_type_column), quote_sql_identifier(source_type_column),
        ), []).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn trim_export_database(snapshot: &Path, thread_ids: &[String], project_ids: &HashSet<String>) -> Result<(), String> {
    let mut conn = Connection::open(snapshot).map_err(|e| format!("无法整理导出状态数据库：{e}"))?;
    conn.execute_batch("PRAGMA foreign_keys = OFF; PRAGMA secure_delete = ON; CREATE TEMP TABLE exported_thread_ids (id TEXT PRIMARY KEY); CREATE TEMP TABLE exported_project_ids (id TEXT PRIMARY KEY);")
        .map_err(|e| format!("无法准备导出状态数据库：{e}"))?;
    {
        let transaction = conn.transaction().map_err(|e| format!("无法整理导出状态数据库：{e}"))?;
        for id in thread_ids {
            transaction.execute("INSERT INTO exported_thread_ids (id) VALUES (?1)", params![id])
                .map_err(|e| format!("无法整理导出会话范围：{e}"))?;
        }
        for id in project_ids {
            transaction.execute("INSERT INTO exported_project_ids (id) VALUES (?1)", params![id])
                .map_err(|e| format!("无法整理导出项目范围：{e}"))?;
        }
        transaction.execute("DELETE FROM thread_spawn_edges WHERE parent_thread_id NOT IN (SELECT id FROM exported_thread_ids) OR child_thread_id NOT IN (SELECT id FROM exported_thread_ids)", [])
            .map_err(|e| format!("无法裁剪子代理关系：{e}"))?;
        transaction.execute("DELETE FROM thread_dynamic_tools WHERE thread_id NOT IN (SELECT id FROM exported_thread_ids)", [])
            .map_err(|e| format!("无法裁剪会话工具配置：{e}"))?;
        for (table, _) in THREAD_ATTACHMENT_TABLES {
            if database_table_exists(&transaction, "main", table)? {
                transaction.execute(&format!("DELETE FROM {} WHERE thread_id NOT IN (SELECT id FROM exported_thread_ids)", quote_sql_identifier(table)), [])
                    .map_err(|e| format!("无法裁剪会话产物配置：{e}"))?;
            }
        }
        transaction.execute("DELETE FROM threads WHERE id NOT IN (SELECT id FROM exported_thread_ids)", [])
            .map_err(|e| format!("无法裁剪会话记录：{e}"))?;
        transaction.execute("DELETE FROM thread_sections WHERE id NOT IN (SELECT DISTINCT thread_section_id FROM threads WHERE thread_section_id IS NOT NULL)", [])
            .map_err(|e| format!("无法裁剪会话分组配置：{e}"))?;
        transaction.execute("DELETE FROM project_roots WHERE project_id NOT IN (SELECT DISTINCT project_id FROM threads WHERE project_id IS NOT NULL) AND project_id NOT IN (SELECT id FROM exported_project_ids)", [])
            .map_err(|e| format!("无法裁剪项目目录配置：{e}"))?;
        transaction.execute("DELETE FROM projects WHERE id NOT IN (SELECT DISTINCT project_id FROM threads WHERE project_id IS NOT NULL) AND id NOT IN (SELECT id FROM exported_project_ids)", [])
            .map_err(|e| format!("无法裁剪项目配置：{e}"))?;
        transaction.commit().map_err(|e| format!("无法提交导出状态裁剪：{e}"))?;
    }
    for table in [
        "_sqlx_migrations", "backfill_state", "external_agent_config_imports", "project_idempotency_keys",
        "remote_control_enrollments", "rollout_migration_skipped_rollouts", "rollout_migration_state",
    ] {
        conn.execute(&format!("DROP TABLE IF EXISTS {table}"), [])
            .map_err(|e| format!("无法清理导出状态数据库：{e}"))?;
    }
    conn.execute_batch("VACUUM").map_err(|e| format!("无法压缩导出状态数据库：{e}"))?;
    Ok(())
}

fn imported_log_destination(home: &Path, batch: &str, thread_id: &str, index: usize) -> PathBuf {
    let timestamp = Local::now() + chrono::Duration::seconds(index as i64);
    home.join("sessions").join("imported").join(batch).join(format!(
        "rollout-{}-{thread_id}.jsonl",
        timestamp.format("%Y-%m-%dT%H-%M-%S")
    ))
}

fn normalize_imported_rollout_paths(home: &Path, db_path: &Path, thread_ids: &HashSet<String>) -> Result<usize, String> {
    let imported_root = home.join("sessions").join("imported");
    let mut plans = Vec::new();
    let mut sequence = 0usize;
    for log in scan_logs(home).into_iter().filter(|log| thread_ids.contains(&log.id) && log.path.starts_with(&imported_root)) {
        let file_name = log.path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if file_name.starts_with("rollout-") { continue; }
        let parent = log.path.parent().ok_or("导入日志路径无效。")?;
        let batch = parent.file_name().and_then(|name| name.to_str()).unwrap_or("imported");
        let mut target = imported_log_destination(home, batch, &log.id, sequence);
        while target.exists() {
            sequence += 1;
            target = imported_log_destination(home, batch, &log.id, sequence);
        }
        plans.push((log.id, log.path, target));
        sequence += 1;
    }
    if plans.is_empty() { return Ok(0); }

    let mut renamed = Vec::new();
    for (_, source, target) in &plans {
        if let Err(error) = fs::rename(source, target) {
            for (original, current) in renamed.iter().rev() { let _ = fs::rename(current, original); }
            return Err(format!("无法将旧版导入日志改为 Codex 标准文件名：{error}"));
        }
        renamed.push((source.clone(), target.clone()));
    }

    let update_result = (|| -> Result<(), String> {
        let mut conn = Connection::open(db_path).map_err(|e| format!("无法打开 Codex 状态数据库：{e}"))?;
        let transaction = conn.transaction().map_err(|e| format!("无法开始更新导入日志路径：{e}"))?;
        for (thread_id, source, target) in &plans {
            let stored: String = transaction.query_row(
                "SELECT rollout_path FROM threads WHERE id = ?1",
                params![thread_id],
                |row| row.get(0),
            ).map_err(|e| format!("无法读取导入会话日志路径：{e}"))?;
            if is_same_path(&stored, &display_path(source)) {
                transaction.execute(
                    "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
                    params![display_path(target), thread_id],
                ).map_err(|e| format!("无法更新导入会话日志路径：{e}"))?;
            }
        }
        transaction.commit().map_err(|e| format!("无法提交导入日志路径更新：{e}"))
    })();
    if let Err(error) = update_result {
        for (original, current) in renamed.iter().rev() { let _ = fs::rename(current, original); }
        return Err(error);
    }
    Ok(plans.len())
}

fn stable_local_project_id(path: &str) -> String {
    let normalized = normalized(path);
    let mut first = DefaultHasher::new();
    normalized.hash(&mut first);
    let mut second = DefaultHasher::new();
    "codex-session-manager".hash(&mut second);
    normalized.hash(&mut second);
    format!("local-{:016x}{:016x}", first.finish(), second.finish())
}

fn prepare_desktop_import_plan(
    home: &Path,
    projects: &[ExportProject],
    requested_mappings: &[ImportProjectMapping],
) -> Result<DesktopImportPlan, String> {
    let state_path = home.join(".codex-global-state.json");
    let mut state = if state_path.is_file() {
        let content = fs::read_to_string(&state_path).map_err(|e| format!("无法读取 Codex 侧栏状态：{e}"))?;
        serde_json::from_str::<Value>(&content).map_err(|e| format!("Codex 侧栏状态格式无效：{e}"))?
    } else {
        Value::Object(serde_json::Map::new())
    };
    let root = state.as_object_mut().ok_or("Codex 侧栏状态格式无效。")?;
    for key in ["local-projects", "app-server-project-id-by-legacy-project-id-by-host"] {
        let value = root.entry(key).or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !value.is_object() { return Err(format!("Codex 侧栏状态字段 {key} 格式无效。")); }
    }
    let order = root.entry("project-order").or_insert_with(|| Value::Array(Vec::new()));
    if !order.is_array() { return Err("Codex 项目顺序状态格式无效。".into()); }

    let mut requested = HashMap::new();
    for mapping in requested_mappings {
        if requested.insert(mapping.project_id.clone(), mapping.target_path.trim().to_string()).is_some() {
            return Err(format!("项目 {} 存在重复的本机目录映射。", mapping.project_id));
        }
    }
    let exported_ids = projects.iter().map(|project| project.id.as_str()).collect::<HashSet<_>>();
    if requested.keys().any(|id| !exported_ids.contains(id.as_str())) {
        return Err("导入请求包含不属于该导出包的项目。".into());
    }

    let host_key = format!("local:{}", display_path(home));
    let mut resolved = HashMap::new();
    for project in projects {
        let requested_path = requested.get(&project.id)
            .ok_or_else(|| format!("请为项目“{}”选择这台电脑上的目录。", project.name))?;
        let target = PathBuf::from(requested_path);
        if !target.is_absolute() || !target.is_dir() {
            return Err(format!("项目“{}”的目标必须是已存在的绝对目录。", project.name));
        }
        let target_path = display_path(&target);
        let existing_desktop_id = root.get("local-projects")
            .and_then(Value::as_object)
            .and_then(|items| items.iter().find_map(|(id, item)| {
                item.get("rootPaths").and_then(Value::as_array)
                    .is_some_and(|paths| paths.iter().filter_map(Value::as_str).any(|path| is_same_path(path, &target_path)))
                    .then_some(id.clone())
            }));
        let desktop_project_id = existing_desktop_id.unwrap_or_else(|| stable_local_project_id(&target_path));
        let existing_app_server_id = root.get("app-server-project-id-by-legacy-project-id-by-host")
            .and_then(Value::as_object)
            .and_then(|hosts| hosts.get(&host_key))
            .and_then(Value::as_object)
            .and_then(|items| items.get(&desktop_project_id))
            .and_then(Value::as_str)
            .map(str::to_string);
        let app_server_project_id = existing_app_server_id.unwrap_or_else(|| project.id.clone());

        let local_projects = root.get_mut("local-projects").and_then(Value::as_object_mut).unwrap();
        if !local_projects.contains_key(&desktop_project_id) {
            let now = Local::now().timestamp_millis();
            local_projects.insert(desktop_project_id.clone(), serde_json::json!({
                "id": desktop_project_id,
                "name": project.name,
                "rootPaths": [target_path],
                "createdAt": now,
                "updatedAt": now,
            }));
            root.get_mut("project-order").and_then(Value::as_array_mut).unwrap()
                .push(Value::String(desktop_project_id.clone()));
        }
        let hosts = root.get_mut("app-server-project-id-by-legacy-project-id-by-host")
            .and_then(Value::as_object_mut).unwrap();
        let host = hosts.entry(host_key.clone()).or_insert_with(|| Value::Object(serde_json::Map::new()));
        let host = host.as_object_mut().ok_or("Codex 项目 ID 映射格式无效。")?;
        host.insert(desktop_project_id.clone(), Value::String(app_server_project_id.clone()));

        resolved.insert(project.id.clone(), ResolvedImportProject {
            source_project_id: project.id.clone(),
            app_server_project_id,
            desktop_project_id,
            target_path,
        });
    }
    Ok(DesktopImportPlan { state_path, state, projects: resolved })
}

fn update_desktop_import_state(
    plan: &mut DesktopImportPlan,
    source_project_by_thread: &HashMap<String, Option<String>>,
) -> Result<usize, String> {
    let root = plan.state.as_object_mut().ok_or("Codex 侧栏状态格式无效。")?;
    let assignments = root.entry("thread-project-assignments")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut().ok_or("Codex 项目归属状态格式无效。")?;
    let mut updated = 0;
    for (thread_id, source_project_id) in source_project_by_thread {
        let Some(project) = source_project_id.as_ref().and_then(|id| plan.projects.get(id)) else { continue };
        assignments.insert(thread_id.clone(), serde_json::json!({
            "projectKind": "local",
            "projectId": project.desktop_project_id,
        }));
        updated += 1;
    }
    Ok(updated)
}

/// Codex Desktop keeps its sidebar project membership separately from the
/// app-server thread table. This preserves membership when a project root is
/// changed, while the conversation itself still has the previous cwd.
fn load_desktop_project_assignments(home: &Path) -> HashMap<String, String> {
    let state_path = home.join(".codex-global-state.json");
    let Ok(raw) = fs::read(&state_path) else { return HashMap::new() };
    let Ok(root) = serde_json::from_slice::<Value>(&raw) else { return HashMap::new() };
    let Some(assignments) = root.get("thread-project-assignments").and_then(Value::as_object) else {
        return HashMap::new();
    };
    let legacy_mappings = root
        .get("app-server-project-id-by-legacy-project-id-by-host")
        .and_then(Value::as_object);
    let mut result = HashMap::new();
    for (thread_id, assignment) in assignments {
        if assignment.get("projectKind").and_then(Value::as_str) != Some("local") { continue; }
        let Some(legacy_id) = assignment.get("projectId").and_then(Value::as_str) else { continue; };
        let current_id = legacy_mappings
            .and_then(|hosts| hosts.values().find_map(|projects| projects.get(legacy_id)))
            .and_then(Value::as_str)
            .unwrap_or(legacy_id);
        result.insert(thread_id.clone(), current_id.to_string());
    }
    result
}

fn effective_project_id(
    thread: &DbThread,
    projects: &[Project],
    desktop_assignments: &HashMap<String, String>,
) -> Option<String> {
    thread.project_id.clone()
        .or_else(|| desktop_assignments.get(&thread.id).cloned())
        .or_else(|| {
            projects
                .iter()
                .find(|project| project.roots.iter().any(|root| is_same_path(root, &thread.cwd)))
                .map(|project| project.id.clone())
        })
}

fn report() -> Result<AuditReport, String> {
    report_at(&codex_home()?)
}

fn report_at(home: &Path) -> Result<AuditReport, String> {
    let db_path = state_database(&home)?;
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("无法打开 Codex 状态数据库：{e}"))?;
    let mut projects = query_projects(&conn)?;
    let all_threads = query_threads(&conn)?;
    let desktop_assignments = load_desktop_project_assignments(&home);
    let logs = scan_logs(&home);
    let mut logs_by_id: HashMap<String, Vec<LogMeta>> = HashMap::new();
    for log in logs.iter().cloned() { logs_by_id.entry(log.id.clone()).or_default().push(log); }

    let project_lookup: HashMap<String, (String, Vec<String>)> = projects
        .iter()
        .map(|p| (p.id.clone(), (p.name.clone(), p.roots.clone())))
        .collect();
    // `source = vscode` is the set that Codex Desktop presents as normal user
    // conversations. Guardian-review and spawned-agent rollouts are persisted in
    // the same table, but are not sidebar sessions and must not inflate a project's
    // displayed count. Keep their IDs below for the history/orphan comparison.
    let thread_ids: HashSet<String> = all_threads.iter().map(|thread| thread.id.clone()).collect();
    let mut sessions = Vec::new();
    let mut matched = 0;
    let mut mismatched = 0;
    let mut unlinked = 0;
    let mut missing_logs = 0;

    for thread in all_threads.into_iter().filter(|thread| thread.source == "vscode") {
        // Prefer Codex Desktop's persistent project assignment. It survives a root
        // migration, which lets the UI show the old cwd as a repairable mismatch.
        let effective_project_id = effective_project_id(&thread, &projects, &desktop_assignments);
        let project = effective_project_id.as_ref().and_then(|id| project_lookup.get(id)).cloned();
        let matching_log = logs_by_id
            .get(&thread.id)
            .and_then(|items| items.iter().find(|log| same_file_path(&log.path, Path::new(&thread.rollout_path))).or_else(|| items.first()));
        let conversation_cwd = matching_log.and_then(|log| log.cwd.clone());
        let log_path = matching_log.map(|log| display_path(&log.path));
        let status = if conversation_cwd.is_none() {
            missing_logs += 1;
            "missing_log"
        } else if project.is_none() {
            unlinked += 1;
            "unlinked"
        } else {
            let roots = &project.as_ref().unwrap().1;
            let same = is_same_path(&thread.cwd, conversation_cwd.as_deref().unwrap());
            let project_matches = roots.iter().any(|root| is_same_path(root, &thread.cwd));
            if same && project_matches {
                matched += 1;
                "match"
            } else {
                mismatched += 1;
                "mismatch"
            }
        }.to_string();
        if let Some(project_id) = &effective_project_id {
            if let Some(project) = projects.iter_mut().find(|project| &project.id == project_id) {
                project.session_count += 1;
                if status == "mismatch" || status == "missing_log" { project.issue_count += 1; }
            }
        }
        sessions.push(Session {
            id: thread.id,
            title: thread.title,
            project_id: effective_project_id,
            project_name: project.as_ref().map(|item| item.0.clone()),
            project_roots: project.map(|item| item.1.into_iter().map(|path| display_path_text(&path)).collect()).unwrap_or_default(),
            database_cwd: display_path_text(&thread.cwd),
            conversation_cwd: conversation_cwd.map(|path| display_path_text(&path)),
            log_path,
            archived: thread.archived,
            status,
        });
    }

    let mut seen_paths = HashSet::new();
    let orphans = logs
        .into_iter()
        .filter(|log| !thread_ids.contains(&log.id) && seen_paths.insert(display_path(&log.path)))
        .map(|log| OrphanRecord {
            id: log.id,
            conversation_cwd: log.cwd.map(|path| display_path_text(&path)),
            log_path: display_path(&log.path),
            archived: log.archived,
        })
        .collect::<Vec<_>>();

    for project in &mut projects {
        for root in &mut project.roots {
            *root = display_path_text(root);
        }
    }

    Ok(AuditReport {
        codex_home: display_path(&home),
        state_database: display_path(&db_path),
        default_backup_directory: display_path(&default_backup_base(&home)),
        summary: Summary {
            projects: projects.len(), sessions: sessions.len(), matched, mismatched, unlinked, missing_logs,
            orphan_records: orphans.len(),
        },
        projects,
        sessions,
        orphans,
    })
}

fn default_backup_base(home: &Path) -> PathBuf {
    home.join("session-manager-backups")
}

fn backup_base(home: &Path, requested: Option<&str>) -> Result<PathBuf, String> {
    let requested = requested.map(str::trim).filter(|value| !value.is_empty());
    let base = requested.map(PathBuf::from).unwrap_or_else(|| default_backup_base(home));
    if !base.is_absolute() { return Err("备份目录必须是绝对路径。".into()); }
    if base.exists() && !base.is_dir() { return Err("备份位置不是目录。".into()); }
    fs::create_dir_all(&base).map_err(|e| format!("无法创建备份目录：{e}"))?;
    Ok(base)
}

fn backup_folder(home: &Path, operation: &str, requested_base: Option<&str>) -> Result<PathBuf, String> {
    let stamp = Local::now().format("%Y%m%d-%H%M%S-%3f");
    let base = backup_base(home, requested_base)?;
    for attempt in 0..1000 {
        let unique_stamp = if attempt == 0 { stamp.to_string() } else { format!("{stamp}{attempt}") };
        let path = base.join(format!("{unique_stamp}-{operation}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("无法创建备份目录：{error}")),
        }
    }
    Err("无法创建独立的备份目录，请稍后重试。".into())
}

fn backup_file(source: &Path, destination: &Path, name: &str) -> Result<(), String> {
    if source.is_file() {
        fs::copy(source, destination.join(name)).map_err(|e| format!("备份 {} 失败：{e}", source.display()))?;
    }
    Ok(())
}

fn backup_database(db_path: &Path, destination: &Path) -> Result<(), String> {
    snapshot_database(db_path, &destination.join("state_5.sqlite"))
}

fn backup_entry(source: &Path, destination: &Path, name: &str) -> Result<Option<BackupFileEntry>, String> {
    if !source.is_file() { return Ok(None); }
    backup_file(source, destination, name)?;
    Ok(Some(BackupFileEntry {
        original_path: display_path(source),
        backup_name: name.to_string(),
    }))
}

fn write_manifest(folder: &Path, manifest: &RepairManifest) -> Result<PathBuf, String> {
    let path = folder.join("repair-history.json");
    let content = serde_json::to_string_pretty(manifest).map_err(|e| format!("无法生成修复历史：{e}"))?;
    fs::write(&path, content).map_err(|e| format!("无法写入修复历史：{e}"))?;
    Ok(path)
}

fn read_manifest(path: &Path) -> Result<RepairManifest, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("无法读取修复历史：{e}"))?;
    serde_json::from_str(&content).map_err(|e| format!("修复历史格式无效：{e}"))
}

fn write_delete_manifest(folder: &Path, manifest: &DeleteManifest) -> Result<PathBuf, String> {
    let path = folder.join("delete-history.json");
    let content = serde_json::to_string_pretty(manifest).map_err(|e| format!("无法生成删除历史：{e}"))?;
    atomic_write(&path, &content).map_err(|e| format!("无法写入删除历史：{e}"))?;
    Ok(path)
}

fn read_delete_manifest(path: &Path) -> Result<DeleteManifest, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("无法读取删除历史：{e}"))?;
    serde_json::from_str(&content).map_err(|e| format!("删除历史格式无效：{e}"))
}

fn repair_history(home: &Path, requested_base: Option<&str>) -> Result<Vec<RepairHistoryItem>, String> {
    let base = backup_base(home, requested_base)?;
    let mut items = Vec::new();
    for entry in fs::read_dir(&base).map_err(|e| format!("无法读取备份目录：{e}"))?.flatten() {
        let folder = entry.path();
        let manifest_path = folder.join("repair-history.json");
        if !folder.is_dir() || !manifest_path.is_file() { continue; }
        let Ok(manifest) = read_manifest(&manifest_path) else { continue };
        items.push(RepairHistoryItem {
            created_at: manifest.created_at,
            thread_id: manifest.thread_id,
            session_title: manifest.session_title,
            source_cwd: manifest.source_cwd,
            target_cwd: manifest.target_cwd,
            backup_folder: display_path(&folder),
            manifest_path: display_path(&manifest_path),
            file_count: manifest.files.len(),
            rolled_back_at: manifest.rolled_back_at,
        });
    }
    items.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(items)
}

fn delete_history(home: &Path, requested_base: Option<&str>) -> Result<Vec<DeleteHistoryItem>, String> {
    let base = backup_base(home, requested_base)?;
    let mut items = Vec::new();
    for entry in fs::read_dir(&base).map_err(|e| format!("无法读取备份目录：{e}"))?.flatten() {
        let folder = entry.path();
        let manifest_path = folder.join("delete-history.json");
        if !folder.is_dir() || !manifest_path.is_file() { continue; }
        let Ok(manifest) = read_delete_manifest(&manifest_path) else { continue };
        let Some(completed_at) = manifest.completed_at else { continue };
        items.push(DeleteHistoryItem {
            created_at: completed_at,
            deletion_kind: manifest.deletion_kind,
            thread_id: manifest.thread_id,
            session_title: manifest.session_title,
            source_path: manifest.source_path,
            child_count: manifest.child_count,
            backup_folder: display_path(&folder),
            file_count: manifest.files.len(),
            rolled_back_at: manifest.rolled_back_at,
        });
    }
    items.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(items)
}

fn backup_history(home: &Path, requested_base: Option<&str>) -> Result<Vec<BackupHistoryItem>, String> {
    let base = backup_base(home, requested_base)?;
    let mut items = Vec::new();
    for entry in fs::read_dir(&base).map_err(|e| format!("无法读取备份目录：{e}"))?.flatten() {
        let folder = entry.path();
        if !folder.is_dir() { continue; }
        let mut file_count = 0;
        let mut total_size = 0;
        for file in WalkDir::new(&folder).follow_links(false).into_iter().flatten().filter(|entry| entry.file_type().is_file()) {
            file_count += 1;
            total_size += file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        }
        let name = folder.file_name().and_then(|part| part.to_str()).unwrap_or("未命名备份").to_string();
        let operation = if name.contains("deleted-session") { "删除会话" } else if name.contains("deleted-orphan") { "删除遗留日志" } else if name.contains("before-import") { "导入前快照" } else if name.contains("repair") { "修复" } else if name.contains("rollback") { "回退前快照" } else { "其他备份" }.to_string();
        let created_at = fs::metadata(&folder).and_then(|metadata| metadata.modified()).ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|time| chrono::DateTime::<chrono::Utc>::from_timestamp(time.as_secs() as i64, 0).map(|time| time.with_timezone(&Local).to_rfc3339()).unwrap_or_else(|| "未知".into()))
            .unwrap_or_else(|| "未知".into());
        items.push(BackupHistoryItem { folder: display_path(&folder), name, operation, created_at, file_count, total_size });
    }
    items.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(items)
}

fn validate_manifest_path(base: &Path, path: &Path) -> Result<PathBuf, String> {
    let base = fs::canonicalize(base).map_err(|e| format!("无法验证备份目录：{e}"))?;
    let path = fs::canonicalize(path).map_err(|e| format!("无法验证修复历史：{e}"))?;
    if path.file_name().and_then(|name| name.to_str()) != Some("repair-history.json") || !path.starts_with(&base) {
        return Err("修复历史不属于当前备份目录。".into());
    }
    Ok(path)
}

fn validate_delete_manifest_path(base: &Path, path: &Path) -> Result<PathBuf, String> {
    let base = fs::canonicalize(base).map_err(|e| format!("无法验证备份目录：{e}"))?;
    let path = fs::canonicalize(path).map_err(|e| format!("无法验证删除历史：{e}"))?;
    if path.file_name().and_then(|name| name.to_str()) != Some("delete-history.json") || !path.starts_with(&base) {
        return Err("删除历史不属于当前备份目录。".into());
    }
    Ok(path)
}

fn validate_backup_folder(base: &Path, path: &Path) -> Result<PathBuf, String> {
    let base = fs::canonicalize(base).map_err(|e| format!("无法验证备份目录：{e}"))?;
    let path = fs::canonicalize(path).map_err(|e| format!("无法验证备份文件：{e}"))?;
    if !path.is_dir() || path.parent() != Some(base.as_path()) {
        return Err("只能删除当前备份目录中的单次备份。".into());
    }
    Ok(path)
}

fn is_restorable_path(home: &Path, db_path: &Path, path: &Path) -> bool {
    path == db_path
        || path == PathBuf::from(format!("{}-wal", db_path.display()))
        || path == PathBuf::from(format!("{}-shm", db_path.display()))
        || path == history_database(home)
        || path == home.join(".codex-global-state.json")
        || is_managed_log(home, path)
}

fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    let file_name = path.file_name().and_then(|name| name.to_str()).ok_or("日志文件名无效")?;
    let temp = path.with_file_name(format!(".{file_name}.session-manager-{}.tmp", std::process::id()));
    fs::write(&temp, content).map_err(|e| format!("写入临时日志失败：{e}"))?;
    if let Err(error) = replace_file(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(format!("替换日志失败：{error}"));
    }
    Ok(())
}

fn purge_thread_reference(value: &mut Value, thread_id: &str) -> usize {
    match value {
        Value::Object(values) => {
            let encoded_thread_id = thread_id.replace('-', "%2D");
            let keys = values.iter()
                .filter(|(key, item)| {
                    key.contains(thread_id)
                        || key.contains(&encoded_thread_id)
                        || (key.contains("new-thread:") && item.as_str() == Some(thread_id))
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            let mut removed = keys.len();
            for key in keys { values.remove(&key); }
            for child in values.values_mut() { removed += purge_thread_reference(child, thread_id); }
            removed
        }
        Value::Array(values) => {
            let before = values.len();
            values.retain(|item| item.as_str() != Some(thread_id));
            let mut removed = before - values.len();
            for child in values.iter_mut() { removed += purge_thread_reference(child, thread_id); }
            removed
        }
        _ => 0,
    }
}

fn purge_global_state_thread_references(home: &Path, thread_ids: &HashSet<String>) -> Result<usize, String> {
    let path = home.join(".codex-global-state.json");
    if !path.is_file() { return Ok(0); }
    let content = fs::read_to_string(&path).map_err(|e| format!("无法读取 Codex 侧栏状态：{e}"))?;
    let mut state: Value = serde_json::from_str(&content).map_err(|e| format!("Codex 侧栏状态格式无效：{e}"))?;
    let removed = thread_ids.iter().map(|id| purge_thread_reference(&mut state, id)).sum();
    if removed > 0 {
        let content = serde_json::to_string(&state).map_err(|e| format!("无法生成更新后的 Codex 侧栏状态：{e}"))?;
        atomic_write(&path, &content)?;
    }
    Ok(removed)
}

fn project_sidebar_threads(home: &Path, conn: &Connection, project_id: &str) -> Result<(String, Vec<String>), String> {
    let projects = query_projects(conn)?;
    let project_name = projects.iter().find(|project| project.id == project_id)
        .map(|project| project.name.clone()).ok_or("找不到目标项目，扫描结果可能已过期。")?;
    let assignments = load_desktop_project_assignments(home);
    let ids = query_threads(conn)?.into_iter()
        .filter(|thread| thread.source == "vscode" && effective_project_id(thread, &projects, &assignments).as_deref() == Some(project_id))
        .map(|thread| thread.id)
        .collect();
    Ok((project_name, ids))
}

fn desktop_legacy_project_ids(state: &Value, app_server_project_id: &str) -> HashSet<String> {
    let mut ids = HashSet::new();
    if state.get("local-projects").and_then(Value::as_object).is_some_and(|projects| projects.contains_key(app_server_project_id)) {
        ids.insert(app_server_project_id.to_string());
    }
    if let Some(hosts) = state.get("app-server-project-id-by-legacy-project-id-by-host").and_then(Value::as_object) {
        for projects in hosts.values().filter_map(Value::as_object) {
            ids.extend(projects.iter().filter_map(|(legacy_id, value)| (value.as_str() == Some(app_server_project_id)).then_some(legacy_id.clone())));
        }
    }
    ids
}

fn update_desktop_project_root(home: &Path, project_id: &str, target: &str) -> Result<usize, String> {
    let path = home.join(".codex-global-state.json");
    if !path.is_file() { return Ok(0); }
    let content = fs::read_to_string(&path).map_err(|e| format!("无法读取 Codex 项目状态：{e}"))?;
    let mut state: Value = serde_json::from_str(&content).map_err(|e| format!("Codex 项目状态格式无效：{e}"))?;
    let ids = desktop_legacy_project_ids(&state, project_id);
    let mut updated = 0;
    if let Some(projects) = state.get_mut("local-projects").and_then(Value::as_object_mut) {
        for id in &ids {
            if let Some(project) = projects.get_mut(id).and_then(Value::as_object_mut) {
                project.insert("rootPaths".into(), serde_json::json!([target]));
                updated += 1;
            }
        }
    }
    if updated > 0 {
        atomic_write(&path, &serde_json::to_string(&state).map_err(|e| format!("无法生成 Codex 项目状态：{e}"))?)?;
    }
    Ok(updated)
}

fn purge_desktop_project(home: &Path, project_id: &str) -> Result<usize, String> {
    let path = home.join(".codex-global-state.json");
    if !path.is_file() { return Ok(0); }
    let content = fs::read_to_string(&path).map_err(|e| format!("无法读取 Codex 项目状态：{e}"))?;
    let mut state: Value = serde_json::from_str(&content).map_err(|e| format!("Codex 项目状态格式无效：{e}"))?;
    let ids = desktop_legacy_project_ids(&state, project_id);
    let root = state.as_object_mut().ok_or("Codex 项目状态格式无效。")?;
    let mut removed = 0;
    if let Some(projects) = root.get_mut("local-projects").and_then(Value::as_object_mut) {
        for id in &ids { if projects.remove(id).is_some() { removed += 1; } }
    }
    if let Some(order) = root.get_mut("project-order").and_then(Value::as_array_mut) {
        let before = order.len();
        order.retain(|value| value.as_str().is_none_or(|id| !ids.contains(id)));
        removed += before - order.len();
    }
    for key in ["project-appearances", "sidebar-project-thread-orders"] {
        if let Some(values) = root.get_mut(key).and_then(Value::as_object_mut) {
            for id in &ids { if values.remove(id).is_some() { removed += 1; } }
        }
    }
    if let Some(hosts) = root.get_mut("app-server-project-id-by-legacy-project-id-by-host").and_then(Value::as_object_mut) {
        for projects in hosts.values_mut().filter_map(Value::as_object_mut) {
            for id in &ids { if projects.remove(id).is_some() { removed += 1; } }
        }
    }
    let selected_matches = root.get("selected-project").and_then(Value::as_object)
        .and_then(|selected| selected.get("projectId")).and_then(Value::as_str)
        .is_some_and(|id| ids.contains(id));
    if selected_matches { root.remove("selected-project"); removed += 1; }
    if removed > 0 {
        atomic_write(&path, &serde_json::to_string(&state).map_err(|e| format!("无法生成 Codex 项目状态：{e}"))?)?;
    }
    Ok(removed)
}

fn purge_desktop_catalog_thread_references(home: &Path, thread_ids: &HashSet<String>) -> Result<usize, String> {
    let path = desktop_catalog_database(home);
    if !path.is_file() { return Ok(0); }
    let mut conn = Connection::open(&path).map_err(|e| format!("无法打开 Codex 桌面目录缓存：{e}"))?;
    let transaction = conn.transaction().map_err(|e| format!("无法开始清理 Codex 桌面目录缓存：{e}"))?;
    let mut removed = 0;
    for thread_id in thread_ids {
        for table in ["local_thread_catalog", "local_thread_catalog_scan_entries", "thread_timeline_ledger", "inbox_items"] {
            let exists: Option<String> = transaction.query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |row| row.get(0),
            ).optional().map_err(|e| format!("无法检查 Codex 桌面目录缓存：{e}"))?;
            if exists.is_some() {
                removed += transaction.execute(&format!("DELETE FROM {table} WHERE thread_id = ?1"), params![thread_id])
                    .map_err(|e| format!("无法清理 Codex 桌面目录缓存：{e}"))?;
            }
        }
    }
    transaction.commit().map_err(|e| format!("无法提交 Codex 桌面目录缓存清理：{e}"))?;
    Ok(removed)
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    let source = source.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
    let destination = destination.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
    let succeeded = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

fn is_codex_process(command: &str) -> bool {
    let command = command.trim().replace('\\', "/").to_lowercase();
    command.contains("/applications/chatgpt.app/contents/macos/chatgpt")
        || command.contains("/applications/codex.app/contents/macos/codex")
        || matches!(command.as_str(), "chatgpt.exe" | "codex.exe" | "codex-code-mode-host.exe" | "chatgpt" | "codex")
        || command.ends_with("/chatgpt.exe")
        || command.ends_with("/codex.exe")
        || command.ends_with("/codex-code-mode-host.exe")
        || (command.contains("codex") && command.contains("app-server"))
}

#[cfg(not(windows))]
fn running_codex_processes() -> Result<Vec<(String, String)>, String> {
    let output = Command::new("ps").args(["-ax", "-o", "pid=,command="]).output()
        .map_err(|e| format!("无法检查 Codex 是否已退出：{e}"))?;
    if !output.status.success() {
        return Err("无法检查 Codex 是否已退出。".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| {
        let mut fields = line.trim().splitn(2, char::is_whitespace);
        let pid = fields.next()?.to_string();
        let command = fields.next()?.trim().to_string();
        is_codex_process(&command).then_some((pid, command))
    }).collect())
}

#[cfg(any(windows, test))]
fn parse_windows_tasklist_line(line: &str) -> Option<(String, String)> {
    let fields = line.trim().trim_matches('"').split("\",\"").collect::<Vec<_>>();
    let image_name = fields.first()?.trim();
    let pid = fields.get(1)?.trim();
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((pid.to_string(), image_name.to_string()))
}

#[cfg(windows)]
fn running_codex_processes() -> Result<Vec<(String, String)>, String> {
    let output = Command::new("tasklist").args(["/FO", "CSV", "/NH"]).output()
        .map_err(|e| format!("无法检查 Codex 是否已退出：{e}"))?;
    if !output.status.success() {
        return Err("无法检查 Codex 是否已退出。".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_windows_tasklist_line)
        .filter(|(_, image_name)| is_codex_process(image_name))
        .collect())
}

#[cfg(not(windows))]
fn request_process_close(pid: &str) -> bool {
    Command::new("kill").args(["-TERM", pid]).status().is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn request_process_close(pid: &str) -> bool {
    Command::new("taskkill").args(["/PID", pid, "/T", "/F"]).status().is_ok_and(|status| status.success())
}

fn ensure_codex_is_closed() -> Result<(), String> {
    let running = running_codex_processes()
        .map_err(|error| format!("{error}，因此已拒绝修改。"))?;
    if !running.is_empty() {
        return Err(format!("检测到 {} 个 Codex 进程仍在运行。请先关闭它们，再重新执行当前操作。", running.len()));
    }
    Ok(())
}

fn codex_executable_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(configured) = std::env::var_os("CODEX_BIN") {
        candidates.push(PathBuf::from(configured));
    }
    if let Some(configured) = std::env::var_os("CODEX_CLI_PATH") {
        candidates.push(PathBuf::from(configured));
    }

    #[cfg(target_os = "macos")]
    {
        candidates.extend([
            PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex"),
            PathBuf::from("/Applications/Codex.app/Contents/Resources/codex-cli/bin/codex"),
            PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex"),
            PathBuf::from("/Applications/Codex.app/Contents/Resources/codex"),
        ]);
        if let Some(user_home) = platform_home_directory() {
            candidates.push(user_home.join("Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex"));
            candidates.push(user_home.join("Applications/Codex.app/Contents/Resources/codex-cli/bin/codex"));
            candidates.push(user_home.join("Applications/ChatGPT.app/Contents/Resources/codex"));
            candidates.push(user_home.join("Applications/Codex.app/Contents/Resources/codex"));
        }
    }

    #[cfg(windows)]
    {
        for base in [std::env::var_os("LOCALAPPDATA"), std::env::var_os("ProgramFiles")].into_iter().flatten() {
            let base = PathBuf::from(base);
            candidates.push(base.join("Programs/ChatGPT/resources/codex-cli/bin/codex.exe"));
            candidates.push(base.join("Programs/Codex/resources/codex-cli/bin/codex.exe"));
            candidates.push(base.join("ChatGPT/resources/codex-cli/bin/codex.exe"));
            candidates.push(base.join("Codex/resources/codex-cli/bin/codex.exe"));
            candidates.push(base.join("Programs/ChatGPT/resources/codex.exe"));
            candidates.push(base.join("Programs/Codex/resources/codex.exe"));
            candidates.push(base.join("ChatGPT/resources/codex.exe"));
            candidates.push(base.join("Codex/resources/codex.exe"));
        }
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let versions = PathBuf::from(local_app_data).join("OpenAI/Codex/bin");
            if let Ok(entries) = fs::read_dir(versions) {
                let mut installed = entries.flatten().map(|entry| entry.path().join("codex.exe"))
                    .filter(|path| path.is_file()).collect::<Vec<_>>();
                installed.sort_by_key(|path| fs::metadata(path).and_then(|metadata| metadata.modified()).ok());
                installed.reverse();
                candidates.extend(installed);
            }
        }
    }

    // Keep PATH as a last resort: a separately installed CLI can be older than
    // the app's bundled app-server protocol.
    candidates.push(PathBuf::from(if cfg!(windows) { "codex.exe" } else { "codex" }));
    candidates
}

#[cfg(test)]
fn parse_thread_delete_response(stdout: &str) -> Result<(), String> {
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        if value.get("id").and_then(Value::as_i64) != Some(2) { continue; }
        if value.get("result").is_some() { return Ok(()); }
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Codex app-server 返回未知删除错误");
        return Err(message.to_string());
    }
    Err("Codex app-server 未返回会话删除结果。".into())
}

fn wait_for_app_server_response(reader: &mut BufReader<std::process::ChildStdout>, request_id: i64) -> Result<Value, String> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).map_err(|error| format!("无法读取 Codex app-server 响应：{error}"))?;
        if read == 0 { return Err(format!("Codex app-server 在响应请求 {request_id} 前已退出。")); }
        let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
        if value.get("id").and_then(Value::as_i64) == Some(request_id) { return Ok(value); }
    }
}

fn delete_thread_with_codex(home: &Path, thread_id: &str) -> Result<(), String> {
    let initialize = serde_json::json!({
        "method": "initialize",
        "id": 1,
        "params": { "clientInfo": { "name": "codex-session-manager", "version": env!("CARGO_PKG_VERSION") } }
    });
    let delete = serde_json::json!({
        "method": "thread/delete",
        "id": 2,
        "params": { "threadId": thread_id }
    });
    let initialized = serde_json::json!({ "method": "initialized" });
    let mut launch_errors = Vec::new();

    for executable in codex_executable_candidates() {
        if executable.components().count() > 1 && !executable.is_file() { continue; }
        let mut child = match Command::new(&executable)
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                launch_errors.push(format!("{}：{error}", executable.display()));
                continue;
            }
        };

        let mut stdin = child.stdin.take().ok_or("无法连接 Codex app-server 标准输入")?;
        let stdout = child.stdout.take().ok_or("无法连接 Codex app-server 标准输出")?;
        let mut reader = BufReader::new(stdout);
        let write_result = writeln!(stdin, "{initialize}").and_then(|_| stdin.flush());
        if let Err(error) = write_result {
            let _ = child.kill();
            return Err(format!("无法向 Codex app-server 发送初始化请求：{error}"));
        }
        let initialize_response = wait_for_app_server_response(&mut reader, 1);
        if let Err(error) = initialize_response {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Codex app-server 初始化失败：{error}"));
        }
        if let Err(error) = writeln!(stdin, "{initialized}").and_then(|_| stdin.flush()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("无法完成 Codex app-server 初始化握手：{error}"));
        }
        let write_result = writeln!(stdin, "{delete}").and_then(|_| stdin.flush());
        if let Err(error) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("无法向 Codex app-server 发送删除请求：{error}"));
        }
        let response = wait_for_app_server_response(&mut reader, 2);
        let _ = child.kill();
        let _ = child.wait();
        let response = response?;
        if response.get("result").is_some() { return Ok(()); }
        let message = response
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Codex app-server 返回未知删除错误");
        return Err(message.to_string());
    }

    let details = if launch_errors.is_empty() { String::new() } else { format!("：{}", launch_errors.join("；")) };
    Err(format!("找不到可用的 Codex app-server{details}。请安装或更新 Codex Desktop，也可通过 CODEX_BIN 指定 codex 可执行文件。"))
}

#[tauri::command]
fn scan_codex() -> Result<AuditReport, String> { report() }

#[tauri::command]
fn inspect_export_package(manifest_path: String) -> Result<ImportPackageInfo, String> {
    let path = fs::canonicalize(Path::new(manifest_path.trim())).map_err(|e| format!("无法读取导出包：{e}"))?;
    let manifest = read_export_manifest(&path)?;
    let root = path.parent().ok_or("导出包路径无效。")?;
    let snapshot = root.join("state_5.sqlite");
    if !snapshot.is_file() { return Err("导出包缺少状态数据库快照。".into()); }
    let history = package_history_path(root, &manifest)?;
    let conn = Connection::open_with_flags(&snapshot, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
    validate_history(&conn, "main", &history, &manifest.thread_ids)?;
    for log in &manifest.logs {
        if !manifest.thread_ids.iter().any(|id| id == &log.thread_id) { return Err("导出包日志归属无效。".into()); }
        let file = safe_package_file(root, &log.file)?;
        if !file.is_file() { return Err(format!("导出包缺少会话日志：{}", log.file)); }
    }
    Ok(ImportPackageInfo {
        manifest_path: display_path(&path),
        exported_at: manifest.exported_at,
        projects: manifest.projects,
        visible_thread_count: manifest.visible_thread_ids.len(),
        thread_count: manifest.thread_ids.len(),
        log_count: manifest.logs.len(),
    })
}

#[tauri::command]
fn export_sessions(request: ExportSessionsRequest) -> Result<ActionResult, String> {
    ensure_codex_is_closed()?;
    export_sessions_at(&codex_home()?, request)
}

fn export_sessions_at(home: &Path, request: ExportSessionsRequest) -> Result<ActionResult, String> {
    if request.thread_ids.is_empty() { return Err("请至少选择一个会话。".into()); }
    let db_path = state_database(&home)?;
    let destination = PathBuf::from(request.destination_directory.trim());
    if !destination.is_absolute() || !destination.is_dir() { return Err("导出位置必须是一个已存在的绝对目录。".into()); }
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("无法读取 Codex 状态数据库：{e}"))?;
    let mut ids = HashSet::new();
    for id in &request.thread_ids {
        let source: String = conn.query_row("SELECT source FROM threads WHERE id = ?1", params![id], |row| row.get(0))
            .map_err(|_| format!("找不到会话 {id}；请重新扫描后再导出。"))?;
        if source != "vscode" { return Err("只能导出 Codex 侧栏中的普通会话。".into()); }
        ids.insert(id.clone());
        ids.extend(descendant_thread_ids(&conn, id)?);
    }
    let mut thread_ids = ids.into_iter().collect::<Vec<_>>();
    thread_ids.sort();
    let projects_for_assignment = query_projects(&conn)?;
    let desktop_assignments = load_desktop_project_assignments(&home);
    let threads_by_id = query_threads(&conn)?.into_iter().map(|thread| (thread.id.clone(), thread)).collect::<HashMap<_, _>>();
    let project_assignments = thread_ids.iter().filter_map(|id| {
        let thread = threads_by_id.get(id)?;
        effective_project_id(thread, &projects_for_assignment, &desktop_assignments).map(|project_id| (id.clone(), project_id))
    }).collect::<HashMap<_, _>>();
    let mut projects = Vec::new();
    let mut project_ids = HashSet::new();
    for id in &thread_ids {
        if let Some(project_id) = project_assignments.get(id) { project_ids.insert(project_id.clone()); }
    }
    for project_id in &project_ids {
        let (id, name): (String, String) = conn.query_row("SELECT id, name FROM projects WHERE id = ?1", params![&project_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| format!("无法读取项目配置：{e}"))?;
        let mut statement = conn.prepare("SELECT path FROM project_roots WHERE project_id = ?1 ORDER BY position")
            .map_err(|e| format!("无法读取项目目录：{e}"))?;
        let roots = statement.query_map(params![&id], |row| row.get::<_, String>(0))
            .map_err(|e| format!("无法读取项目目录：{e}"))?
            .collect::<Result<Vec<_>, _>>().map_err(|e| format!("无法读取项目目录：{e}"))?;
        projects.push(ExportProject { id, name, roots });
    }
    projects.sort_by(|left, right| left.name.cmp(&right.name));
    // Two SQLite databases and JSONL files must describe the same stopped store.
    validate_history(&conn, "main", &history_database(&home), &thread_ids)?;
    let available_logs = scan_logs(&home);
    let mut logs = Vec::new();
    for id in &thread_ids {
        let thread = threads_by_id.get(id).ok_or("导出会话信息不完整。")?;
        let log = available_logs.iter().find(|log| log.id == *id && same_file_path(&log.path, Path::new(&thread.rollout_path)))
            .ok_or_else(|| format!("会话 {id} 缺少当前数据库引用的 JSONL 日志，无法创建完整导出包。"))?;
        logs.push(log.clone());
    }
    let stamp = Local::now().format("%Y%m%d-%H%M%S");
    let package = destination.join(format!("codex-session-export-{stamp}"));
    fs::create_dir(&package).map_err(|e| format!("无法创建导出包目录：{e}"))?;
    let logs_folder = package.join("logs");
    fs::create_dir(&logs_folder).map_err(|e| format!("无法创建导出日志目录：{e}"))?;
    backup_database(&db_path, &package)?;
    trim_export_database(&package.join("state_5.sqlite"), &thread_ids, &project_ids)?;
    let history = snapshot_history(&home, &package, &thread_ids)?;
    let history_conn = if history.is_some() { Some(Connection::open(package.join(HISTORY_FILE)).map_err(|e| e.to_string())?) } else { None };
    let mut exported_logs = Vec::new();
    for (index, log) in logs.iter().enumerate() {
        let name = format!("{index}-{}.jsonl", log.id);
        let exported = logs_folder.join(&name);
        fs::copy(&log.path, &exported).map_err(|e| format!("无法导出会话日志：{e}"))?;
        if let Some(history) = &history_conn {
            let unchanged = prepare_log_rewrite(&exported, &HashSet::new(), "")?;
            remap_history_offsets(history, "main", &log.id, &unchanged)?;
        }
        exported_logs.push(ExportLog { thread_id: log.id.clone(), file: format!("logs/{name}"), archived: log.archived, canonical: true });
    }
    let mut visible_thread_ids = request.thread_ids.clone();
    visible_thread_ids.sort();
    visible_thread_ids.dedup();
    let exported_path_settings = desktop_state_update(&home)?.map(|desktop| thread_ids.iter()
        .map(|id| (id.clone(), thread_path_settings(&desktop.state, id))).collect()).unwrap_or_default();
    let manifest = SessionExportManifest {
        format: "codex-session-manager-export".into(),
        version: 2,
        exported_at: Local::now().to_rfc3339(),
        projects,
        visible_thread_ids,
        thread_ids,
        project_assignments,
        logs: exported_logs,
        history_database: history.map(|entry| entry.backup_name),
        thread_path_settings: exported_path_settings,
    };
    let manifest_path = package.join("manifest.json");
    fs::write(&manifest_path, serde_json::to_string_pretty(&manifest).map_err(|e| format!("无法生成导出清单：{e}"))?)
        .map_err(|e| format!("无法写入导出清单：{e}"))?;
    Ok(ActionResult {
        message: "已导出会话包。导入时请选择包内的 manifest.json。".into(),
        backup_folder: display_path(&package),
        changes: vec![
            format!("导出项目配置：{} 个", manifest.projects.len()),
            format!("导出会话：{} 个主会话，{} 个会话记录", manifest.visible_thread_ids.len(), manifest.thread_ids.len()),
            format!("导出对话日志：{} 个", manifest.logs.len()),
        ],
    })
}

#[tauri::command]
fn import_sessions(request: ImportSessionsRequest) -> Result<ActionResult, String> {
    ensure_codex_is_closed()?;
    import_sessions_at(&codex_home()?, request)
}

fn import_sessions_at(home: &Path, request: ImportSessionsRequest) -> Result<ActionResult, String> {
    if request.confirmation != "IMPORT" { return Err("导入确认无效。".into()); }
    let manifest_path = fs::canonicalize(Path::new(request.manifest_path.trim())).map_err(|e| format!("无法读取导出包：{e}"))?;
    let manifest = read_export_manifest(&manifest_path)?;
    let package = manifest_path.parent().ok_or("导出包路径无效。")?;
    let package_db = package.join("state_5.sqlite");
    if !package_db.is_file() { return Err("导出包缺少状态数据库快照。".into()); }
    let package_conn = Connection::open_with_flags(&package_db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("无法打开导出包数据库：{e}"))?;
    let package_history = package_history_path(package, &manifest)?;
    validate_history(&package_conn, "main", &package_history, &manifest.thread_ids)?;
    let paginated = paginated_threads(&package_conn, "main", &manifest.thread_ids)?;
    let mut source_project_by_thread = HashMap::new();
    let mut source_cwd_by_thread = HashMap::new();
    for id in &manifest.thread_ids {
        let (stored_project_id, source_cwd): (Option<String>, String) = package_conn.query_row(
            "SELECT project_id, cwd FROM threads WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
            .map_err(|_| format!("导出包数据库中找不到会话 {id}。"))?;
        let project_id = stored_project_id.or_else(|| manifest.project_assignments.get(id).cloned());
        source_project_by_thread.insert(id.clone(), project_id);
        source_cwd_by_thread.insert(id.clone(), source_cwd);
    }
    let mut logs_by_thread: HashMap<String, Vec<(PathBuf, bool)>> = HashMap::new();
    for log in &manifest.logs {
        if !source_project_by_thread.contains_key(&log.thread_id) { return Err("导出包日志归属无效。".into()); }
        let source = safe_package_file(package, &log.file)?;
        if !source.is_file() { return Err(format!("导出包缺少会话日志：{}", log.file)); }
        let parsed = parse_log(&source, log.archived).ok_or_else(|| format!("导出包日志格式无效：{}", log.file))?;
        if parsed.id != log.thread_id { return Err(format!("导出包日志 ID 不匹配：{}", log.file)); }
        logs_by_thread.entry(log.thread_id.clone()).or_default().push((source, log.archived));
    }
    for visible_id in &manifest.visible_thread_ids {
        if !logs_by_thread.contains_key(visible_id) { return Err(format!("导出包中的主会话 {visible_id} 缺少日志。")); }
    }
    let mut canonical_files = HashMap::new();
    for id in &manifest.thread_ids {
        let entries = manifest.logs.iter().filter(|log| log.thread_id == *id).collect::<Vec<_>>();
        let marked = entries.iter().filter(|log| log.canonical).collect::<Vec<_>>();
        let canonical = if marked.len() == 1 { Some(*marked[0]) } else if marked.is_empty() && entries.len() == 1 { Some(entries[0]) } else { None };
        if let Some(log) = canonical { canonical_files.insert(id.clone(), safe_package_file(package, &log.file)?); }
        else if paginated.contains(id) { return Err(format!("分页会话 {id} 缺少唯一的当前日志，已取消导入。")); }
    }
    let mut desktop_plan = prepare_desktop_import_plan(&home, &manifest.projects, &request.project_mappings)?;
    let resolved_projects = desktop_plan.projects.clone();
    for project_id in source_project_by_thread.values().flatten() {
        if !resolved_projects.contains_key(project_id) {
            return Err(format!("导出包中的项目 {project_id} 缺少本机目录映射。"));
        }
    }
    let db_path = state_database(&home)?;
    let mut conn = Connection::open(&db_path).map_err(|e| format!("无法打开 Codex 状态数据库：{e}"))?;
    if !paginated.is_empty() && !database_table_columns(&conn, "main", "threads")?.iter().any(|column| column == "history_mode") {
        return Err("当前 Codex 版本不支持分页历史，请升级 Codex 后再导入。".into());
    }
    for id in &manifest.thread_ids {
        let existing: Option<String> = conn.query_row("SELECT id FROM threads WHERE id = ?1", params![id], |row| row.get(0)).optional()
            .map_err(|e| format!("无法检查现有会话：{e}"))?;
        if existing.is_some() { return Err(format!("当前 Codex 已存在会话 {id}；为避免覆盖，已取消导入。")); }
    }
    let batch = Local::now().format("%Y%m%d-%H%M%S-%3f-import").to_string();
    let imported_logs_root = home.join("sessions").join("imported").join(&batch);
    fs::create_dir_all(&imported_logs_root).map_err(|e| format!("无法创建导入会话目录：{e}"))?;
    let mut rollout_paths: HashMap<String, String> = HashMap::new();
    let mut copied_logs = 0;
    let mut rewrites = Vec::new();
    let mut canonical_rewrites = HashMap::new();
    let mut previous_paths_by_thread = HashMap::new();
    let copy_result = (|| -> Result<(), String> {
        for (thread_id, logs) in &logs_by_thread {
            for (index, (source, _)) in logs.iter().enumerate() {
                let destination = imported_log_destination(&home, &batch, thread_id, index);
                fs::copy(source, &destination).map_err(|e| format!("无法写入导入会话日志：{e}"))?;
                let mut previous_paths = HashSet::new();
                if let Some(project) = source_project_by_thread.get(thread_id).and_then(Option::as_ref).and_then(|id| resolved_projects.get(id)) {
                    if let Some(cwd) = source_cwd_by_thread.get(thread_id) { previous_paths.insert(cwd.clone()); }
                    if let Some(parsed) = parse_log(&destination, false).and_then(|log| log.cwd) { previous_paths.insert(parsed); }
                    if let Some(exported) = manifest.projects.iter().find(|item| item.id == project.source_project_id) {
                        previous_paths.extend(exported.roots.iter().cloned());
                    }
                }
                let target = source_project_by_thread.get(thread_id).and_then(Option::as_ref).and_then(|id| resolved_projects.get(id)).map(|project| project.target_path.as_str()).unwrap_or("");
                let rewrite = prepare_log_rewrite(&destination, &previous_paths, target)?;
                if canonical_files.get(thread_id) == Some(source) {
                    canonical_rewrites.insert(thread_id.clone(), rewrites.len());
                    rollout_paths.insert(thread_id.clone(), display_path(&destination));
                } else { rollout_paths.entry(thread_id.clone()).or_insert_with(|| display_path(&destination)); }
                previous_paths_by_thread.insert(thread_id.clone(), previous_paths);
                rewrites.push(rewrite);
                copied_logs += 1;
            }
        }
        Ok(())
    })();
    if let Err(error) = copy_result {
        let _ = fs::remove_dir_all(&imported_logs_root);
        return Err(error);
    }
    let prepare_desktop = (|| -> Result<DesktopStateUpdate, String> {
        for id in &manifest.thread_ids {
            if let Some(settings) = manifest.thread_path_settings.get(id) {
                let mut settings = settings.clone();
                if let Some(project) = source_project_by_thread.get(id).and_then(Option::as_ref).and_then(|id| resolved_projects.get(id)) {
                    for value in settings.values_mut() { remap_paths(value, previous_paths_by_thread.get(id).ok_or("导入会话缺少路径信息。")?, &project.target_path); }
                }
                validate_desktop_state(&desktop_plan.state)?;
                restore_thread_settings(&mut desktop_plan.state, id, &settings);
            }
        }
        update_desktop_import_state(&mut desktop_plan, &source_project_by_thread)?;
        validate_desktop_state(&desktop_plan.state)?;
        let path = desktop_plan.state_path.clone();
        let existed = path.exists();
        let original = if existed { fs::read_to_string(&path).map_err(|e| e.to_string())? } else { String::new() };
        Ok(DesktopStateUpdate { path, original, existed, state: desktop_plan.state.clone() })
    })();
    let desktop = match prepare_desktop {
        Ok(desktop) => desktop,
        Err(error) => { let _ = fs::remove_dir_all(&imported_logs_root); return Err(error); }
    };
    let assignment_count = source_project_by_thread.values().flatten().count();
    let result = (|| -> Result<(), String> {
        let has_history = attach_history(&conn, &home, Some(&package_history))?;
        conn.execute("ATTACH DATABASE ?1 AS imported_package", params![package_db.to_string_lossy().as_ref()])
            .map_err(|e| format!("无法连接导出包数据库：{e}"))?;
        conn.execute_batch("CREATE TEMP TABLE importing_thread_ids (id TEXT PRIMARY KEY); CREATE TEMP TABLE importing_project_ids (id TEXT PRIMARY KEY)")
            .map_err(|e| format!("无法准备导入会话：{e}"))?;
        for id in &manifest.thread_ids {
            conn.execute("INSERT INTO importing_thread_ids (id) VALUES (?1)", params![id])
                .map_err(|e| format!("无法准备导入会话：{e}"))?;
        }
        for project in resolved_projects.values().filter(|project| project.app_server_project_id == project.source_project_id) {
            conn.execute("INSERT OR IGNORE INTO importing_project_ids (id) VALUES (?1)", params![project.source_project_id])
                .map_err(|e| format!("无法准备导入项目：{e}"))?;
        }
        let transaction = conn.transaction().map_err(|e| format!("无法开始导入：{e}"))?;
        copy_database_rows(&transaction, "imported_package", "projects", "id IN (SELECT id FROM importing_project_ids)", true)
            .map_err(|e| format!("无法导入项目配置：{e}"))?;
        copy_database_rows(&transaction, "imported_package", "project_roots", "project_id IN (SELECT id FROM importing_project_ids)", true)
            .map_err(|e| format!("无法导入项目目录配置：{e}"))?;
        for project in resolved_projects.values().filter(|project| project.app_server_project_id == project.source_project_id) {
            transaction.execute("DELETE FROM project_roots WHERE project_id = ?1", params![project.app_server_project_id])
                .map_err(|e| format!("无法替换导入项目目录：{e}"))?;
            transaction.execute(
                "INSERT INTO project_roots (project_id, position, path) VALUES (?1, 0, ?2)",
                params![project.app_server_project_id, project.target_path],
            ).map_err(|e| format!("无法写入导入项目目录：{e}"))?;
        }
        copy_database_rows(&transaction, "imported_package", "thread_sections", "id IN (SELECT DISTINCT thread_section_id FROM imported_package.threads WHERE id IN (SELECT id FROM importing_thread_ids) AND thread_section_id IS NOT NULL)", true)
            .map_err(|e| format!("无法导入会话分组配置：{e}"))?;
        copy_database_rows(&transaction, "imported_package", "threads", "id IN (SELECT id FROM importing_thread_ids)", false)
            .map_err(|e| format!("无法导入会话记录：{e}"))?;
        copy_database_rows(&transaction, "imported_package", "thread_dynamic_tools", "thread_id IN (SELECT id FROM importing_thread_ids)", false)
            .map_err(|e| format!("无法导入会话工具配置：{e}"))?;
        copy_thread_attachments(&transaction, "imported_package", "importing_thread_ids")
            .map_err(|e| format!("无法导入会话产物配置：{e}"))?;
        copy_database_rows(&transaction, "imported_package", "thread_spawn_edges", "child_thread_id IN (SELECT id FROM importing_thread_ids)", true)
            .map_err(|e| format!("无法导入子代理关系：{e}"))?;
        if has_history && package_history.is_file() {
            copy_history(&transaction, "importing_thread_ids")?;
            for (id, index) in &canonical_rewrites { remap_history_offsets(&transaction, "thread_history", id, &rewrites[*index])?; }
        }
        for (thread_id, rollout_path) in &rollout_paths {
            transaction.execute("UPDATE threads SET rollout_path = ?1 WHERE id = ?2", params![rollout_path, thread_id])
                .map_err(|e| format!("无法更新导入会话日志位置：{e}"))?;
        }
        for (thread_id, source_project_id) in &source_project_by_thread {
            if let Some(project) = source_project_id.as_ref().and_then(|id| resolved_projects.get(id)) {
                update_thread_path(&transaction, thread_id, previous_paths_by_thread.get(thread_id).ok_or("导入会话缺少路径信息。")?, &project.target_path, None)?;
                transaction.execute(
                    "UPDATE threads SET project_id = ?1, cwd = ?2 WHERE id = ?3",
                    params![project.app_server_project_id, project.target_path, thread_id],
                )
                    .map_err(|e| format!("无法更新导入会话项目归属：{e}"))?;
            }
        }
        for project in resolved_projects.values().filter(|project| project.app_server_project_id != project.source_project_id) {
            transaction.execute(
                "DELETE FROM project_roots WHERE project_id = ?1 AND NOT EXISTS (SELECT 1 FROM threads WHERE project_id = ?1)",
                params![project.source_project_id],
            ).map_err(|e| format!("无法清理源电脑的项目目录配置：{e}"))?;
            transaction.execute(
                "DELETE FROM projects WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM threads WHERE project_id = ?1)",
                params![project.source_project_id],
            ).map_err(|e| format!("无法清理源电脑的空项目配置：{e}"))?;
        }
        commit_with_logs(transaction, &rewrites, Some(&desktop)).map_err(|e| format!("无法提交导入：{e}"))?;
        conn.execute_batch("DETACH DATABASE imported_package").ok();
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&imported_logs_root);
        return Err(error);
    }
    Ok(ActionResult {
        message: "已导入会话及项目配置，并将工作目录转换为这台电脑上的项目目录。请重新打开 Codex，导入的会话会显示在对应项目中。".into(),
        backup_folder: String::new(),
        changes: vec![
            format!("导入会话记录：{} 个", manifest.thread_ids.len()),
            format!("导入项目配置：{} 个", manifest.projects.len()),
            format!("导入对话日志：{copied_logs} 个"),
            format!("更新侧栏项目归属：{assignment_count} 条"),
        ],
    })
}

#[tauri::command]
fn prepare_backup_directory(backup_base: Option<String>) -> Result<String, String> {
    let home = codex_home()?;
    Ok(display_path(&crate::backup_base(&home, backup_base.as_deref())?))
}

#[tauri::command]
fn list_repair_history(backup_base: Option<String>) -> Result<Vec<RepairHistoryItem>, String> {
    let home = codex_home()?;
    repair_history(&home, backup_base.as_deref())
}

#[tauri::command]
fn list_delete_history(backup_base: Option<String>) -> Result<Vec<DeleteHistoryItem>, String> {
    let home = codex_home()?;
    delete_history(&home, backup_base.as_deref())
}

#[tauri::command]
fn list_backup_history(backup_base: Option<String>) -> Result<Vec<BackupHistoryItem>, String> {
    let home = codex_home()?;
    backup_history(&home, backup_base.as_deref())
}

#[tauri::command]
fn cleanup_deleted_sidebar_references(backup_base: Option<String>) -> Result<usize, String> {
    let home = codex_home()?;
    let ids = delete_history(&home, backup_base.as_deref())?.into_iter()
        .filter(|item| item.deletion_kind == "session")
        .filter(|item| item.rolled_back_at.is_none())
        .filter_map(|item| item.thread_id)
        .collect::<HashSet<_>>();
    let global_removed = purge_global_state_thread_references(&home, &ids)?;
    let catalog_removed = purge_desktop_catalog_thread_references(&home, &ids)?;
    Ok(global_removed + catalog_removed)
}

#[tauri::command]
fn delete_backup(request: BackupDeleteRequest) -> Result<(), String> {
    if request.confirmation != "DELETE_BACKUP" { return Err("删除备份确认无效。".into()); }
    let home = codex_home()?;
    let base = backup_base(&home, request.backup_base.as_deref())?;
    let folder = validate_backup_folder(&base, Path::new(&request.backup_folder))?;
    fs::remove_dir_all(&folder).map_err(|e| format!("删除备份失败：{e}"))
}

#[tauri::command]
fn clear_history(request: ClearHistoryRequest) -> Result<usize, String> {
    if request.confirmation != "CLEAR_HISTORY" { return Err("清除历史确认无效。".into()); }
    if !matches!(request.kind.as_str(), "repair" | "delete" | "backup") { return Err("未知的历史类型。".into()); }
    let home = codex_home()?;
    let base = backup_base(&home, request.backup_base.as_deref())?;
    let mut folders = Vec::new();
    for entry in fs::read_dir(&base).map_err(|e| format!("无法读取备份目录：{e}"))?.flatten() {
        let folder = entry.path();
        if !folder.is_dir() { continue; }
        let matches_kind = match request.kind.as_str() {
            "repair" => folder.join("repair-history.json").is_file(),
            "delete" => folder.join("delete-history.json").is_file(),
            "backup" => true,
            _ => false,
        };
        if matches_kind { folders.push(validate_backup_folder(&base, &folder)?); }
    }
    let count = folders.len();
    for folder in folders { fs::remove_dir_all(&folder).map_err(|e| format!("删除备份失败：{e}"))?; }
    Ok(count)
}

#[tauri::command]
fn close_codex_processes() -> Result<ProcessCloseResult, String> {
    let mut running = running_codex_processes()?;
    if running.is_empty() {
        return Ok(ProcessCloseResult { message: "没有检测到正在运行的 Codex 进程。".into(), requested: 0, remaining: 0 });
    }
    let mut requested = 0;
    for _ in 0..3 {
        for (pid, _) in &running {
            if request_process_close(pid) {
                requested += 1;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        running = running_codex_processes()?;
        if running.is_empty() { break; }
    }
    let remaining = running.len();
    let message = if remaining == 0 {
        format!("已结束 Codex 进程树，共完成 {requested} 次关闭操作。现在可以重新执行修复、导入或删除。")
    } else {
        let names = running.iter().map(|(_, name)| name.as_str()).collect::<HashSet<_>>().into_iter().collect::<Vec<_>>().join("、");
        format!("已执行 {requested} 次关闭操作，但仍检测到 {remaining} 个 Codex 进程（{names}）。如果它们由 VS Code 扩展重新启动，请先完全退出 VS Code，再重试。")
    };
    Ok(ProcessCloseResult {
        message,
        requested,
        remaining,
    })
}

#[tauri::command]
fn repair_session(request: RepairRequest) -> Result<ActionResult, String> {
    ensure_codex_is_closed()?;
    repair_session_at(&codex_home()?, request)
}

fn repair_session_at(home: &Path, request: RepairRequest) -> Result<ActionResult, String> {
    if request.confirmation != "REPAIR" { return Err("请在确认框输入 REPAIR。".into()); }
    let target = PathBuf::from(request.target_path.trim());
    if !target.is_absolute() || !target.is_dir() { return Err("修复目标必须是一个已存在的绝对目录。".into()); }
    let db_path = state_database(&home)?;
    let mut conn = Connection::open(&db_path).map_err(|e| format!("无法打开状态数据库：{e}"))?;
    let mut logs_by_id: HashMap<String, Vec<LogMeta>> = HashMap::new();
    for log in scan_logs(&home) {
        logs_by_id.entry(log.id.clone()).or_default().push(log);
    }
    let targets = load_repair_targets(&conn, &request.thread_id, request.include_child_agents, &logs_by_id)?;
    let root = targets.first().ok_or("未找到目标会话。")?;
    let ids = targets.iter().map(|target| target.id.clone()).collect::<Vec<_>>();
    validate_history(&conn, "main", &history_database(&home), &ids)?;
    let has_history = attach_history(&conn, &home, None)?;
    let mut desktop = desktop_state_update(&home)?;
    let backup = backup_folder(&home, "repair", request.backup_base.as_deref())?;
    backup_database(&db_path, &backup)?;
    let mut backup_files = vec![BackupFileEntry { original_path: display_path(&db_path), backup_name: "state_5.sqlite".into() }];
    if let Some(entry) = snapshot_history(&home, &backup, &ids)? { backup_files.push(entry); }
    if let Some(entry) = backup_entry(&home.join(".codex-global-state.json"), &backup, "codex-global-state.json")? { backup_files.push(entry); }
    let mut log_index = 0;
    for target in &targets {
        for log in &target.logs {
            let name = format!("{log_index}-{}", log.path.file_name().unwrap_or_default().to_string_lossy());
            if let Some(entry) = backup_entry(&log.path, &backup, &name)? { backup_files.push(entry); }
            log_index += 1;
        }
    }
    let target_text = display_path(&target);
    let mut replacements = 0;
    let mut changes = Vec::new();
    let mut thread_changes = Vec::new();
    let mut rewrites = Vec::new();
    let mut canonical_rewrites = HashMap::new();
    let mut previous_paths_by_thread = HashMap::new();
    for repair_target in &targets {
        let mut old_paths = HashSet::from([repair_target.cwd.clone()]);
        for log in &repair_target.logs {
            if let Some(cwd) = &log.cwd { old_paths.insert(cwd.clone()); }
        }
        let label = if repair_target.id == request.thread_id { "主会话" } else { "子代理" };
        changes.push(format!("{label}数据库工作目录（{}）：{} → {}", repair_target.title, repair_target.cwd, target_text));
        for log in &repair_target.logs {
            let rewrite = prepare_log_rewrite(&log.path, &old_paths, &target_text)?;
            replacements += rewrite.replacements;
            if same_file_path(&log.path, Path::new(&repair_target.rollout_path)) { canonical_rewrites.insert(repair_target.id.clone(), rewrites.len()); }
            rewrites.push(rewrite);
            let name = log.path.file_name().unwrap_or_default().to_string_lossy();
            if let Some(cwd) = &log.cwd {
                changes.push(format!("{label}对话工作目录（{name}）：{cwd} → {target_text}"));
            }
        }
        let desktop_path_settings = desktop.as_ref().map(|desktop| thread_path_settings(&desktop.state, &repair_target.id)).unwrap_or_default();
        if let Some(desktop) = &mut desktop {
            let mut updated = desktop_path_settings.clone();
            for value in updated.values_mut() { remap_paths(value, &old_paths, &target_text); }
            restore_thread_settings(&mut desktop.state, &repair_target.id, &updated);
        }
        let sandbox_policy = conn.query_row("SELECT sandbox_policy FROM threads WHERE id = ?1", params![repair_target.id], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?;
        previous_paths_by_thread.insert(repair_target.id.clone(), old_paths);
        thread_changes.push(ThreadRepairChange {
            thread_id: repair_target.id.clone(),
            session_title: repair_target.title.clone(),
            source_cwd: repair_target.cwd.clone(),
            target_cwd: target_text.clone(),
            sandbox_policy: Some(sandbox_policy),
            desktop_path_settings,
        });
    }
    let manifest = RepairManifest {
        version: 3,
        created_at: Local::now().to_rfc3339(),
        thread_id: request.thread_id,
        session_title: root.title.clone(),
        source_cwd: root.cwd.clone(),
        target_cwd: target_text.clone(),
        files: backup_files,
        thread_changes,
        rolled_back_at: None,
    };
    write_manifest(&backup, &manifest)?;
    let transaction = conn.transaction().map_err(|e| format!("无法开始会话路径更新：{e}"))?;
    for repair_target in &targets {
        update_thread_path(&transaction, &repair_target.id, &previous_paths_by_thread[&repair_target.id], &target_text, None)
            .map_err(|e| format!("更新会话路径失败（备份位于 {}）：{e}", backup.display()))?;
        if has_history {
            remap_history_offsets(&transaction, "thread_history", &repair_target.id, &rewrites[canonical_rewrites[&repair_target.id]])?;
        }
    }
    commit_with_logs(transaction, &rewrites, desktop.as_ref()).map_err(|e| format!("无法提交路径修复（备份位于 {}）：{e}", backup.display()))?;
    let child_count = targets.len().saturating_sub(1);
    let scope = if child_count == 0 { "主会话".to_string() } else { format!("主会话及 {child_count} 个子代理") };
    Ok(ActionResult { message: format!("已修复{scope}的路径，更新日志中的 {replacements} 处目录和权限字段，并同步分页历史索引。请重启 Codex 后复查。"), backup_folder: display_path(&backup), changes })
}

#[tauri::command]
fn repair_project(request: ProjectRepairRequest) -> Result<ActionResult, String> {
    if request.confirmation != "REPAIR_PROJECT" { return Err("项目修复确认无效。".into()); }
    let target = PathBuf::from(request.target_path.trim());
    if !target.is_absolute() || !target.is_dir() { return Err("项目修复目标必须是一个已存在的绝对目录。".into()); }
    ensure_codex_is_closed()?;
    let home = codex_home()?;
    let db_path = state_database(&home)?;
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("无法打开 Codex 状态数据库：{e}"))?;
    let (project_name, thread_ids) = project_sidebar_threads(&home, &conn, &request.project_id)?;
    let logs_by_id = scan_logs(&home).into_iter().map(|log| log.id).collect::<HashSet<_>>();
    let mut repairable = Vec::new();
    let mut repair_scope = HashSet::new();
    let mut skipped = 0;
    for thread_id in &thread_ids {
        let mut scope = vec![thread_id.clone()];
        scope.extend(descendant_thread_ids(&conn, thread_id)?);
        if scope.iter().all(|id| logs_by_id.contains(id)) {
            repair_scope.extend(scope);
            repairable.push(thread_id.clone());
        } else { skipped += 1; }
    }
    let scoped_ids = repair_scope.iter().cloned().collect::<Vec<_>>();
    validate_history(&conn, "main", &history_database(&home), &scoped_ids)?;
    drop(conn);

    let safety = backup_folder(&home, "repair-project", request.backup_base.as_deref())?;
    backup_database(&db_path, &safety)?;
    snapshot_history(&home, &safety, &scoped_ids)?;
    let _ = backup_entry(&home.join(".codex-global-state.json"), &safety, "codex-global-state.json")?;
    for (index, log) in scan_logs(&home).into_iter().filter(|log| repair_scope.contains(&log.id)).enumerate() {
        let name = format!("{index}-{}", log.path.file_name().unwrap_or_default().to_string_lossy());
        backup_file(&log.path, &safety, &name)?;
    }
    let mut completed = 0;
    for thread_id in &repairable {
        repair_session(RepairRequest {
            thread_id: thread_id.clone(),
            target_path: display_path(&target),
            confirmation: "REPAIR".into(),
            backup_base: request.backup_base.clone(),
            include_child_agents: true,
        }).map_err(|error| format!("项目“{project_name}”已修复 {completed} 个主会话，处理会话 {thread_id} 时失败；项目操作前快照位于 {}：{error}", safety.display()))?;
        completed += 1;
    }

    let target_text = display_path(&target);
    let mut conn = Connection::open(&db_path).map_err(|e| format!("无法打开 Codex 状态数据库：{e}"))?;
    let transaction = conn.transaction().map_err(|e| format!("无法开始更新项目目录：{e}"))?;
    transaction.execute("DELETE FROM project_roots WHERE project_id = ?1", params![&request.project_id])
        .map_err(|e| format!("无法清理旧项目目录：{e}"))?;
    transaction.execute("INSERT INTO project_roots (project_id, position, path) VALUES (?1, 0, ?2)", params![&request.project_id, &target_text])
        .map_err(|e| format!("无法写入项目目录：{e}"))?;
    transaction.commit().map_err(|e| format!("无法提交项目目录更新：{e}"))?;
    let desktop_projects = update_desktop_project_root(&home, &request.project_id, &target_text)?;
    let mut changes = vec![
        format!("统一项目目录：{target_text}"),
        format!("修复主会话：{completed} 个（同时包含其子代理）"),
        format!("更新 Codex Desktop 项目配置：{desktop_projects} 项"),
    ];
    if skipped > 0 { changes.push(format!("因主会话或子代理缺少 JSONL 日志而跳过：{skipped} 个")); }
    Ok(ActionResult {
        message: format!("已将项目“{project_name}”统一到指定目录。已修复 {completed} 个主会话；跳过 {skipped} 个缺少完整日志的会话。请重启 Codex 后复查。"),
        backup_folder: display_path(&safety),
        changes,
    })
}

#[tauri::command]
fn rollback_repair(request: RollbackRequest) -> Result<ActionResult, String> {
    ensure_codex_is_closed()?;
    rollback_repair_at(&codex_home()?, request)
}

fn rollback_repair_at(home: &Path, request: RollbackRequest) -> Result<ActionResult, String> {
    if request.confirmation != "ROLLBACK" { return Err("回退确认无效。".into()); }
    let db_path = state_database(&home)?;
    let base = backup_base(&home, request.backup_base.as_deref())?;
    let manifest_path = validate_manifest_path(&base, Path::new(&request.manifest_path))?;
    let mut manifest = read_manifest(&manifest_path)?;
    if !(1..=3).contains(&manifest.version) || manifest.files.is_empty() { return Err("该修复历史不支持回退。".into()); }
    let folder = manifest_path.parent().ok_or("修复历史目录无效")?;
    for entry in &manifest.files {
        let original = PathBuf::from(&entry.original_path);
        let source = folder.join(&entry.backup_name);
        if !is_restorable_path(&home, &db_path, &original) || !source.is_file() {
            return Err(format!("修复历史包含无效或缺失的备份文件：{}", entry.backup_name));
        }
    }

    let thread_changes = if manifest.thread_changes.is_empty() {
        vec![ThreadRepairChange {
            thread_id: manifest.thread_id.clone(),
            session_title: manifest.session_title.clone(),
            source_cwd: manifest.source_cwd.clone(),
            target_cwd: manifest.target_cwd.clone(),
            sandbox_policy: None,
            desktop_path_settings: HashMap::new(),
        }]
    } else {
        manifest.thread_changes.clone()
    };
    let change_by_id: HashMap<String, ThreadRepairChange> = thread_changes
        .iter().cloned().map(|change| (change.thread_id.clone(), change)).collect();
    let current_logs = scan_logs(&home).into_iter()
        .filter(|log| change_by_id.contains_key(&log.id))
        .collect::<Vec<_>>();
    let found_ids = current_logs.iter().map(|log| log.id.clone()).collect::<HashSet<_>>();
    if let Some(missing) = thread_changes.iter().find(|change| !found_ids.contains(&change.thread_id)) {
        return Err(format!("找不到会话“{}”当前的 JSONL 日志，已拒绝回退。", missing.session_title));
    }

    let safety = backup_folder(&home, "before-rollback", request.backup_base.as_deref())?;
    backup_database(&db_path, &safety)?;
    let ids = thread_changes.iter().map(|change| change.thread_id.clone()).collect::<Vec<_>>();
    snapshot_history(&home, &safety, &ids)?;
    backup_entry(&home.join(".codex-global-state.json"), &safety, "codex-global-state.json")?;
    for (index, log) in current_logs.iter().enumerate() {
        let name = format!("current-{index}-{}", log.path.file_name().unwrap_or_default().to_string_lossy());
        backup_file(&log.path, &safety, &name)?;
    }

    let mut conn = Connection::open(&db_path).map_err(|e| format!("无法打开状态数据库：{e}"))?;
    validate_history(&conn, "main", &history_database(&home), &ids)?;
    let has_history = attach_history(&conn, &home, None)?;
    let mut desktop = desktop_state_update(&home)?;
    let mut rewrites = Vec::new();
    let mut canonical_rewrites = HashMap::new();
    for log in &current_logs {
        let change = &change_by_id[&log.id];
        let baseline = manifest.files.iter().find(|entry| same_file_path(Path::new(&entry.original_path), &log.path))
            .map(|entry| folder.join(&entry.backup_name));
        let rewrite = prepare_log_rollback(&log.path, baseline.as_deref(), &change.target_cwd, &change.source_cwd)?;
        let canonical: String = conn.query_row("SELECT rollout_path FROM threads WHERE id = ?1", params![log.id], |row| row.get(0)).map_err(|e| e.to_string())?;
        if same_file_path(&log.path, Path::new(&canonical)) { canonical_rewrites.insert(log.id.clone(), rewrites.len()); }
        rewrites.push(rewrite);
    }
    if let Some(desktop) = &mut desktop {
        for change in &thread_changes { restore_thread_settings(&mut desktop.state, &change.thread_id, &change.desktop_path_settings); }
    }
    let transaction = conn.transaction().map_err(|e| format!("无法开始回退：{e}"))?;
    for change in &thread_changes {
        update_thread_path(&transaction, &change.thread_id, &HashSet::from([change.target_cwd.clone()]), &change.source_cwd, change.sandbox_policy.as_deref())?;
        if has_history {
            let index = canonical_rewrites.get(&change.thread_id).ok_or("找不到当前数据库引用的日志，已停止回退。")?;
            remap_history_offsets(&transaction, "thread_history", &change.thread_id, &rewrites[*index])?;
        }
    }
    let replacements = rewrites.iter().map(|rewrite| rewrite.replacements).sum::<usize>();
    commit_with_logs(transaction, &rewrites, desktop.as_ref()).map_err(|e| format!("无法提交回退（回退前备份位于 {}）：{e}", safety.display()))?;
    manifest.rolled_back_at = Some(Local::now().to_rfc3339());
    write_manifest(folder, &manifest)?;
    Ok(ActionResult {
        message: "已回退到本次修复前的状态。请重新启动 Codex 后复查。".into(),
        backup_folder: display_path(&safety),
        changes: vec![
            format!("数据库工作目录：{} → {}", manifest.target_cwd, manifest.source_cwd),
            format!("对话日志工作目录字段：恢复 {replacements} 处"),
            format!("回退前的当前状态已备份到：{}", safety.display()),
        ],
    })
}

#[tauri::command]
fn rollback_delete(request: DeleteRollbackRequest) -> Result<ActionResult, String> {
    ensure_codex_is_closed()?;
    rollback_delete_at(&codex_home()?, request)
}

fn rollback_delete_at(home: &Path, request: DeleteRollbackRequest) -> Result<ActionResult, String> {
    if request.confirmation != "RESTORE_DELETION" { return Err("删除回退确认无效。".into()); }
    let db_path = state_database(&home)?;
    let base = backup_base(&home, request.backup_base.as_deref())?;
    let manifest_path = validate_delete_manifest_path(&base, Path::new(&request.manifest_path))?;
    let mut manifest = read_delete_manifest(&manifest_path)?;
    if manifest.completed_at.is_none() { return Err("该删除操作未完成，不能回退。".into()); }
    let folder = manifest_path.parent().ok_or("删除历史目录无效")?;
    let safety = backup_folder(&home, "before-delete-rollback", request.backup_base.as_deref())?;
    backup_database(&db_path, &safety)?;

    if manifest.deletion_kind == "orphan" {
        let entry = manifest.files.iter().find(|entry| entry.backup_name != "state_5.sqlite")
            .ok_or("删除备份中没有可恢复的日志文件。")?;
        let original = PathBuf::from(&entry.original_path);
        let source = folder.join(&entry.backup_name);
        if !is_managed_log(&home, &original) || !source.is_file() { return Err("删除备份中的日志文件无效或缺失。".into()); }
        if original.exists() { backup_file(&original, &safety, &format!("current-{}", entry.backup_name))?; }
        fs::create_dir_all(original.parent().ok_or("日志路径无效")?).map_err(|e| format!("无法创建日志目录：{e}"))?;
        fs::copy(&source, &original).map_err(|e| format!("恢复日志失败：{e}"))?;
        manifest.rolled_back_at = Some(Local::now().to_rfc3339());
        write_delete_manifest(folder, &manifest)?;
        return Ok(ActionResult { message: "已恢复删除的遗留日志。请重新扫描确认。".into(), backup_folder: display_path(&safety), changes: vec![format!("恢复日志：{}", original.display())] });
    }

    let thread_id = manifest.thread_id.clone().ok_or("删除历史缺少会话 ID。")?;
    let snapshot = folder.join("state_5.sqlite");
    if !snapshot.is_file() { return Err("删除备份中缺少状态数据库，不能回退。".into()); }
    let mut conn = Connection::open(&db_path).map_err(|e| format!("无法打开当前状态数据库：{e}"))?;
    conn.execute("ATTACH DATABASE ?1 AS deleted_backup", params![snapshot.to_string_lossy().as_ref()])
        .map_err(|e| format!("无法打开删除前的状态数据库备份：{e}"))?;
    conn.execute_batch("CREATE TEMP TABLE restored_thread_ids (id TEXT PRIMARY KEY)")
        .map_err(|e| format!("无法准备恢复会话：{e}"))?;
    let ids = {
        let mut statement = conn.prepare(
            "WITH RECURSIVE restored(id) AS (SELECT ?1 UNION SELECT edge.child_thread_id FROM deleted_backup.thread_spawn_edges edge JOIN restored ON edge.parent_thread_id = restored.id) SELECT id FROM restored"
        ).map_err(|e| format!("无法读取删除备份：{e}"))?;
        let ids = statement.query_map(params![&thread_id], |row| row.get::<_, String>(0))
            .map_err(|e| format!("无法读取删除会话范围：{e}"))?
            .collect::<Result<Vec<_>, _>>().map_err(|e| format!("无法读取删除会话范围：{e}"))?;
        ids
    };
    if ids.is_empty() { return Err("删除备份中找不到该会话。".into()); }
    for id in &ids {
        let exists: Option<String> = conn.query_row("SELECT id FROM threads WHERE id = ?1", params![id], |row| row.get(0)).optional()
            .map_err(|e| format!("无法检查当前会话：{e}"))?;
        if exists.is_some() { return Err("当前数据库已存在同 ID 会话，已拒绝覆盖。".into()); }
        conn.execute("INSERT INTO restored_thread_ids (id) VALUES (?1)", params![id])
            .map_err(|e| format!("无法准备恢复会话：{e}"))?;
    }
    let snapshot_history = folder.join(HISTORY_FILE);
    validate_history(&conn, "deleted_backup", &snapshot_history, &ids)?;
    if !paginated_threads(&conn, "deleted_backup", &ids)?.is_empty() && !database_table_columns(&conn, "main", "threads")?.iter().any(|column| column == "history_mode") {
        return Err("当前 Codex 不支持备份中的分页历史，请升级 Codex 后恢复。".into());
    }
    compatibility::snapshot_history(&home, &safety, &ids)?;
    let has_history = attach_history(&conn, &home, Some(&snapshot_history))?;
    backup_entry(&home.join(".codex-global-state.json"), &safety, "codex-global-state.json")?;
    let mut desktop = desktop_state_update(&home)?;
    let backup_state_path = folder.join("codex-global-state.json");
    let backup_state: Value = if backup_state_path.is_file() {
        serde_json::from_str(&fs::read_to_string(&backup_state_path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
    } else { serde_json::json!({}) };
    conn.execute_batch("CREATE TEMP TABLE restored_project_ids (id TEXT PRIMARY KEY); INSERT OR IGNORE INTO restored_project_ids SELECT project_id FROM deleted_backup.threads WHERE id IN (SELECT id FROM restored_thread_ids) AND project_id IS NOT NULL")
        .map_err(|e| e.to_string())?;
    let mut restored_projects = HashSet::new();
    for id in &ids {
        if let Some(project) = desktop_project_for_thread(&backup_state, id) {
            conn.execute("INSERT OR IGNORE INTO restored_project_ids VALUES (?1)", params![project]).map_err(|e| e.to_string())?;
            restored_projects.insert(project);
        }
    }
    if let Some(desktop) = &mut desktop {
        restore_desktop_projects(&mut desktop.state, &backup_state, &restored_projects);
        for id in &ids {
            restore_thread_settings(&mut desktop.state, id, &thread_path_settings(&backup_state, id));
            if let Some(assignment) = backup_state.get("thread-project-assignments").and_then(|assignments| assignments.get(id)) {
                if desktop.state.get("thread-project-assignments").is_none() { desktop.state["thread-project-assignments"] = serde_json::json!({}); }
                desktop.state["thread-project-assignments"][id] = assignment.clone();
            }
        }
    }
    let log_entries = manifest.files.iter().filter(|entry| is_managed_log(&home, Path::new(&entry.original_path))).cloned().collect::<Vec<_>>();
    let mut rewrites = Vec::new();
    for entry in &log_entries {
        let source = folder.join(&entry.backup_name);
        if !source.is_file() { return Err(format!("删除备份缺少日志：{}", entry.backup_name)); }
        let original = PathBuf::from(&entry.original_path);
        if original.exists() { backup_file(&original, &safety, &format!("current-{}", entry.backup_name))?; }
        fs::create_dir_all(original.parent().ok_or("日志路径无效")?).map_err(|e| e.to_string())?;
        rewrites.push(prepare_log_restore(&source, &original)?);
    }
    {
        let transaction = conn.transaction().map_err(|e| format!("无法开始删除回退：{e}"))?;
        copy_database_rows(&transaction, "deleted_backup", "projects", "id IN (SELECT id FROM restored_project_ids)", true)?;
        copy_database_rows(&transaction, "deleted_backup", "project_roots", "project_id IN (SELECT id FROM restored_project_ids)", true)?;
        copy_database_rows(&transaction, "deleted_backup", "thread_sections", "id IN (SELECT thread_section_id FROM deleted_backup.threads WHERE id IN (SELECT id FROM restored_thread_ids))", true)?;
        copy_database_rows(&transaction, "deleted_backup", "threads", "id IN (SELECT id FROM restored_thread_ids)", false)
            .map_err(|e| format!("恢复会话记录失败：{e}"))?;
        copy_database_rows(&transaction, "deleted_backup", "thread_dynamic_tools", "thread_id IN (SELECT id FROM restored_thread_ids)", false).map_err(|e| format!("恢复会话工具失败：{e}"))?;
        copy_thread_attachments(&transaction, "deleted_backup", "restored_thread_ids").map_err(|e| format!("恢复会话产物失败：{e}"))?;
        copy_database_rows(&transaction, "deleted_backup", "thread_spawn_edges", "child_thread_id IN (SELECT id FROM restored_thread_ids)", true).map_err(|e| format!("恢复会话关系失败：{e}"))?;
        if has_history && snapshot_history.is_file() {
            copy_history(&transaction, "restored_thread_ids")?;
        }
        for id in &ids {
            let rollout: String = transaction.query_row("SELECT rollout_path FROM threads WHERE id = ?1", params![id], |row| row.get(0)).map_err(|e| e.to_string())?;
            let rewrite = rewrites.iter().find(|rewrite| same_file_path(&rewrite.path, Path::new(&rollout))).ok_or_else(|| format!("删除备份缺少会话 {id} 的当前日志，已取消恢复。"))?;
            if has_history { remap_history_offsets(&transaction, "thread_history", id, rewrite)?; }
        }
        commit_with_logs(transaction, &rewrites, desktop.as_ref()).map_err(|e| format!("无法提交删除回退：{e}"))?;
    }
    conn.execute_batch("DETACH DATABASE deleted_backup").ok();
    manifest.rolled_back_at = Some(Local::now().to_rfc3339());
    write_delete_manifest(folder, &manifest)?;
    Ok(ActionResult { message: format!("已恢复会话“{}”及其 {} 个子代理。请重新打开 Codex 后确认。", manifest.session_title, ids.len().saturating_sub(1)), backup_folder: display_path(&safety), changes: vec![format!("恢复会话记录：{} 个", ids.len()), format!("恢复对话日志：{} 个", log_entries.len())] })
}

#[tauri::command]
fn delete_session(request: DeleteSessionRequest) -> Result<ActionResult, String> {
    if request.confirmation != "DELETE" { return Err("请在确认框输入 DELETE。".into()); }
    // Codex Desktop keeps its sidebar in memory.  Deleting through a separate
    // app-server process while it is open leaves a stale row that cannot be
    // restored, even though the database deletion succeeded.
    ensure_codex_is_closed()?;
    delete_session_at(&codex_home()?, request)
}

fn delete_session_at(home: &Path, request: DeleteSessionRequest) -> Result<ActionResult, String> {
    if request.confirmation != "DELETE" { return Err("请在确认框输入 DELETE。".into()); }
    let db_path = state_database(&home)?;
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("无法打开 Codex 状态数据库：{e}"))?;
    let (title, source): (String, String) = conn.query_row(
        "SELECT COALESCE(NULLIF(TRIM(name), ''), title), source FROM threads WHERE id = ?1",
        params![&request.thread_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(|e| format!("找不到要删除的会话，扫描结果可能已过期：{e}"))?;
    if source != "vscode" { return Err("只能删除 Codex 侧边栏中的普通会话。".into()); }

    let mut thread_ids = HashSet::from([request.thread_id.clone()]);
    thread_ids.extend(descendant_thread_ids(&conn, &request.thread_id)?);
    let scoped_ids = thread_ids.iter().cloned().collect::<Vec<_>>();
    validate_history(&conn, "main", &history_database(&home), &scoped_ids)?;
    drop(conn);

    let backup = backup_folder(&home, "deleted-session", request.backup_base.as_deref())?;
    backup_database(&db_path, &backup)?;
    let normalized_rollouts = normalize_imported_rollout_paths(&home, &db_path, &thread_ids)
        .map_err(|error| format!("删除前修复旧版导入日志失败；操作前备份位于 {}：{error}", backup.display()))?;
    if normalized_rollouts > 0 {
        // Keep the deletion snapshot internally consistent with the renamed files,
        // so rolling the deletion back restores a session Codex can open again.
        backup_database(&db_path, &backup)?;
    }
    let mut backup_files = vec![BackupFileEntry {
        original_path: display_path(&db_path),
        backup_name: "state_5.sqlite".into(),
    }];
    if let Some(entry) = snapshot_history(&home, &backup, &scoped_ids)? { backup_files.push(entry); }
    if let Some(entry) = backup_entry(&home.join(".codex-global-state.json"), &backup, "codex-global-state.json")? {
        backup_files.push(entry);
    }
    let catalog_db = desktop_catalog_database(&home);
    if let Some(entry) = backup_entry(&catalog_db, &backup, "codex-dev.db")? {
        backup_files.push(entry);
    }
    let logs = scan_logs(&home).into_iter()
        .filter(|log| thread_ids.contains(&log.id))
        .collect::<Vec<_>>();
    for (index, log) in logs.iter().enumerate() {
        let name = format!("{index}-{}", log.path.file_name().unwrap_or_default().to_string_lossy());
        if let Some(entry) = backup_entry(&log.path, &backup, &name)? { backup_files.push(entry); }
    }

    let child_count = thread_ids.len().saturating_sub(1);
    let mut manifest = DeleteManifest {
        version: 2,
        created_at: Local::now().to_rfc3339(),
        completed_at: None,
        deletion_kind: "session".into(),
        thread_id: Some(request.thread_id.clone()),
        session_title: title.clone(),
        source_path: None,
        child_count,
        files: backup_files,
        rolled_back_at: None,
    };
    write_delete_manifest(&backup, &manifest)?;

    delete_thread_with_codex(&home, &request.thread_id)
        .map_err(|error| format!("删除会话失败；操作前备份位于 {}：{error}", backup.display()))?;
    let sidebar_references = purge_global_state_thread_references(&home, &thread_ids)
        .map_err(|error| format!("会话已从状态库删除，但无法清理 Codex 侧栏状态；操作前备份位于 {}：{error}", backup.display()))?;
    let catalog_references = purge_desktop_catalog_thread_references(&home, &thread_ids)
        .map_err(|error| format!("会话已从状态库删除，但无法清理 Codex 桌面目录缓存；操作前备份位于 {}：{error}", backup.display()))?;
    manifest.completed_at = Some(Local::now().to_rfc3339());
    write_delete_manifest(&backup, &manifest)?;
    let child_note = if child_count == 0 { String::new() } else { format!("及其 {child_count} 个子代理") };
    Ok(ActionResult {
        message: format!("已通过 Codex 删除会话“{title}”{child_note}，并清理 {sidebar_references} 条侧栏状态引用和 {catalog_references} 条桌面目录缓存。重新打开 Codex 后即可看到结果。"),
        backup_folder: display_path(&backup),
        changes: {
            let mut changes = vec![format!("删除的会话：{}（{}）", title, request.thread_id)];
            if normalized_rollouts > 0 { changes.push(format!("兼容修复旧版导入日志文件名：{normalized_rollouts} 个")); }
            changes
        },
    })
}

#[tauri::command]
fn delete_project(request: ProjectDeleteRequest) -> Result<ActionResult, String> {
    if request.confirmation != "DELETE_PROJECT" { return Err("项目删除确认无效。".into()); }
    ensure_codex_is_closed()?;
    let home = codex_home()?;
    let db_path = state_database(&home)?;
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("无法打开 Codex 状态数据库：{e}"))?;
    let (project_name, thread_ids) = project_sidebar_threads(&home, &conn, &request.project_id)?;
    let mut all_thread_ids = HashSet::new();
    for thread_id in &thread_ids {
        all_thread_ids.insert(thread_id.clone());
        all_thread_ids.extend(descendant_thread_ids(&conn, thread_id)?);
    }
    drop(conn);

    let safety = backup_folder(&home, "before-project-delete", request.backup_base.as_deref())?;
    backup_database(&db_path, &safety)?;
    snapshot_history(&home, &safety, &all_thread_ids.iter().cloned().collect::<Vec<_>>())?;
    let _ = backup_entry(&home.join(".codex-global-state.json"), &safety, "codex-global-state.json")?;
    let _ = backup_entry(&desktop_catalog_database(&home), &safety, "codex-dev.db")?;
    for (index, log) in scan_logs(&home).into_iter().filter(|log| all_thread_ids.contains(&log.id)).enumerate() {
        let name = format!("{index}-{}", log.path.file_name().unwrap_or_default().to_string_lossy());
        let _ = backup_entry(&log.path, &safety, &name)?;
    }

    let mut completed = 0;
    for thread_id in &thread_ids {
        delete_session(DeleteSessionRequest {
            thread_id: thread_id.clone(),
            confirmation: "DELETE".into(),
            backup_base: request.backup_base.clone(),
        }).map_err(|error| format!("项目“{project_name}”已删除 {completed} 个主会话，处理会话 {thread_id} 时失败；项目操作前快照位于 {}：{error}", safety.display()))?;
        completed += 1;
    }

    let mut conn = Connection::open(&db_path).map_err(|e| format!("无法打开 Codex 状态数据库：{e}"))?;
    let transaction = conn.transaction().map_err(|e| format!("无法开始删除项目配置：{e}"))?;
    let remaining: i64 = transaction.query_row("SELECT COUNT(*) FROM threads WHERE project_id = ?1", params![&request.project_id], |row| row.get(0))
        .map_err(|e| format!("无法检查项目残留会话：{e}"))?;
    if remaining > 0 { return Err(format!("项目仍有 {remaining} 条非侧栏会话记录，已保留项目配置；项目操作前快照位于 {}。", safety.display())); }
    transaction.execute("DELETE FROM project_roots WHERE project_id = ?1", params![&request.project_id])
        .map_err(|e| format!("无法删除项目目录配置：{e}"))?;
    transaction.execute("DELETE FROM projects WHERE id = ?1", params![&request.project_id])
        .map_err(|e| format!("无法删除项目配置：{e}"))?;
    transaction.commit().map_err(|e| format!("无法提交项目删除：{e}"))?;
    let desktop_references = purge_desktop_project(&home, &request.project_id)?;
    Ok(ActionResult {
        message: format!("已删除项目“{project_name}”及其 {completed} 个主会话，并清理 {desktop_references} 条桌面项目引用。磁盘上的项目源代码目录未被删除。请重新打开 Codex。"),
        backup_folder: display_path(&safety),
        changes: vec![
            format!("删除主会话：{completed} 个（同时包含其子代理）"),
            "删除 Codex 项目配置和项目目录记录".into(),
            format!("清理桌面项目引用：{desktop_references} 条"),
        ],
    })
}

#[tauri::command]
fn delete_orphan(request: DeleteRequest) -> Result<ActionResult, String> {
    if request.confirmation != "DELETE" { return Err("请在确认框输入 DELETE。".into()); }
    ensure_codex_is_closed()?;
    let home = codex_home()?;
    let target = PathBuf::from(&request.log_path);
    if !target.is_file() || !is_managed_log(&home, &target) { return Err("目标不是 Codex 会话目录中的 JSONL 文件。".into()); }
    let current = report()?;
    if !current.orphans.iter().any(|record| record.log_path == request.log_path) {
        return Err("此记录仍被 Codex 当前数据库引用，或扫描结果已过期；请重新扫描。".into());
    }
    let backup = backup_folder(&home, "deleted-orphan", request.backup_base.as_deref())?;
    let backup_name = target.file_name().and_then(|item| item.to_str()).unwrap_or("orphan.jsonl");
    let backup_entry = backup_entry(&target, &backup, backup_name)?.ok_or("无法备份要删除的遗留日志。")?;
    let mut manifest = DeleteManifest {
        version: 1,
        created_at: Local::now().to_rfc3339(),
        completed_at: None,
        deletion_kind: "orphan".into(),
        thread_id: None,
        session_title: target.file_name().and_then(|item| item.to_str()).unwrap_or("遗留日志").into(),
        source_path: Some(display_path(&target)),
        child_count: 0,
        files: vec![backup_entry],
        rolled_back_at: None,
    };
    write_delete_manifest(&backup, &manifest)?;
    fs::remove_file(&target).map_err(|e| format!("删除遗留日志失败：{e}"))?;
    manifest.completed_at = Some(Local::now().to_rfc3339());
    write_delete_manifest(&backup, &manifest)?;
    Ok(ActionResult { message: "已从 Codex 会话目录删除该未引用日志；原文件已备份。".into(), backup_folder: display_path(&backup), changes: vec![format!("删除的遗留日志：{}", target.display())] })
}

fn update_zoom<R: tauri::Runtime>(app: &tauri::AppHandle<R>, requested: f64) {
    let zoom = requested.clamp(MIN_ZOOM, MAX_ZOOM);
    if let Ok(mut current) = app.state::<ZoomLevel>().0.lock() {
        *current = zoom;
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_zoom(zoom);
    }
}

fn current_zoom<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> f64 {
    app.state::<ZoomLevel>()
        .0
        .lock()
        .map(|zoom| *zoom)
        .unwrap_or(DEFAULT_ZOOM)
}

fn set_predefined_menu_text<R: tauri::Runtime>(
    submenu: &tauri::menu::Submenu<R>,
    position: usize,
    text: &str,
) -> tauri::Result<()> {
    if let Some(MenuItemKind::Predefined(item)) = submenu.items()?.get(position) {
        item.set_text(text)?;
    }
    Ok(())
}

fn application_menu<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<Menu<R>> {
    let menu = Menu::default(app)?;

    #[cfg(target_os = "macos")]
    if let Some(MenuItemKind::Submenu(application_submenu)) = menu.items()?.into_iter().next() {
        // Replace the generic About item so the native panel receives the
        // product identity explicitly, including a Dock-sized icon and credits.
        application_submenu.remove_at(0)?;
        let metadata = AboutMetadata {
            name: Some("Codex Session Manager".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            copyright: Some("Copyright © 2026 H-Knight".into()),
            credits: Some("作者：H-Knight".into()),
            icon: Some(tauri::image::Image::from_bytes(include_bytes!(
                "../icons/icon-macos.png"
            ))?),
            ..Default::default()
        };
        let about = PredefinedMenuItem::about(
            app,
            Some("关于 Codex Session Manager"),
            Some(metadata),
        )?;
        application_submenu.prepend(&about)?;
        set_predefined_menu_text(&application_submenu, 2, "服务")?;
        set_predefined_menu_text(&application_submenu, 4, "隐藏 Codex Session Manager")?;
        set_predefined_menu_text(&application_submenu, 5, "隐藏其他应用")?;
        set_predefined_menu_text(&application_submenu, 7, "退出 Codex Session Manager")?;
    }

    let zoom_in = MenuItemBuilder::with_id("view.zoom-in", "放大")
        .accelerator("CmdOrCtrl+Equal")
        .build(app)?;
    let zoom_out = MenuItemBuilder::with_id("view.zoom-out", "缩小")
        .accelerator("CmdOrCtrl+Minus")
        .build(app)?;
    let reset_zoom = MenuItemBuilder::with_id("view.zoom-reset", "实际大小")
        .accelerator("CmdOrCtrl+0")
        .build(app)?;

    let mut view_submenu = None;
    for item in menu.items()? {
        let MenuItemKind::Submenu(submenu) = item else { continue };
        match submenu.text()?.as_str() {
            "File" => {
                submenu.set_text("文件")?;
                set_predefined_menu_text(&submenu, 0, "关闭窗口")?;
                #[cfg(not(target_os = "macos"))]
                set_predefined_menu_text(&submenu, 1, "退出 Codex Session Manager")?;
            }
            "Edit" => {
                submenu.set_text("编辑")?;
                set_predefined_menu_text(&submenu, 0, "撤销")?;
                set_predefined_menu_text(&submenu, 1, "重做")?;
                set_predefined_menu_text(&submenu, 3, "剪切")?;
                set_predefined_menu_text(&submenu, 4, "复制")?;
                set_predefined_menu_text(&submenu, 5, "粘贴")?;
                set_predefined_menu_text(&submenu, 6, "全选")?;
            }
            "View" => {
                submenu.set_text("显示")?;
                set_predefined_menu_text(&submenu, 0, "进入全屏幕")?;
                view_submenu = Some(submenu);
            }
            "Window" => {
                submenu.set_text("窗口")?;
                set_predefined_menu_text(&submenu, 0, "最小化")?;
                set_predefined_menu_text(&submenu, 1, "缩放")?;
                let close_position = submenu.items()?.len().saturating_sub(1);
                set_predefined_menu_text(&submenu, close_position, "关闭窗口")?;
            }
            "Help" => {
                submenu.set_text("帮助")?;
                #[cfg(not(target_os = "macos"))]
                set_predefined_menu_text(&submenu, 0, "关于 Codex Session Manager")?;
            }
            _ => {}
        }
    }

    let zoom_items: [&dyn tauri::menu::IsMenuItem<R>; 3] = [&zoom_in, &zoom_out, &reset_zoom];
    if let Some(view) = view_submenu {
        view.insert_items(&zoom_items, 0)?;
    } else {
        let view = SubmenuBuilder::new(app, "显示").items(&zoom_items).build()?;
        menu.append(&view)?;
    }
    Ok(menu)
}

pub fn run() {
    tauri::Builder::default()
        .manage(ZoomLevel(Mutex::new(DEFAULT_ZOOM)))
        .menu(application_menu)
        .on_menu_event(|app, event| {
            if event.id() == "view.zoom-in" {
                update_zoom(app, current_zoom(app) + ZOOM_STEP);
            } else if event.id() == "view.zoom-out" {
                update_zoom(app, current_zoom(app) - ZOOM_STEP);
            } else if event.id() == "view.zoom-reset" {
                update_zoom(app, DEFAULT_ZOOM);
            }
        })
        // Register first so a second launch exits before it can create another window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![scan_codex, inspect_export_package, export_sessions, import_sessions, prepare_backup_directory, list_repair_history, list_delete_history, list_backup_history, cleanup_deleted_sidebar_references, delete_backup, clear_history, close_codex_processes, repair_session, repair_project, rollback_repair, rollback_delete, delete_session, delete_project, delete_orphan])
        .run(tauri::generate_context!())
        .expect("error while running Codex Session Manager");
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TemporaryDatabase {
        folder: PathBuf,
        path: PathBuf,
    }

    impl TemporaryDatabase {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let serial = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let folder = std::env::temp_dir().join(format!(
                "codex-session-manager-schema-test-{}-{}-{serial}",
                std::process::id(), Local::now().timestamp_nanos_opt().unwrap_or_default()
            ));
            fs::create_dir(&folder).unwrap();
            let path = folder.join("state_5.sqlite");
            Self { folder, path }
        }
    }

    impl Drop for TemporaryDatabase {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.folder); }
    }

    fn create_attachment_table(conn: &Connection, schema: &str, table: &str, type_column: &str) {
        conn.execute_batch(&format!(
            "CREATE TABLE {schema}.{table} (
                id TEXT PRIMARY KEY,
                thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
                {type_column} TEXT NOT NULL,
                identity_key TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                UNIQUE (thread_id, {type_column}, identity_key)
            );"
        )).unwrap();
    }

    #[test]
    fn trims_export_attachments_for_current_legacy_and_absent_tables() {
        for tables in [
            vec![("thread_artifacts", "artifact_type")],
            vec![("thread_attachments", "attachment_type")],
            THREAD_ATTACHMENT_TABLES.to_vec(),
            vec![],
        ] {
            let database = TemporaryDatabase::new();
            let conn = Connection::open(&database.path).unwrap();
            conn.execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE projects (id TEXT PRIMARY KEY);
                 CREATE TABLE project_roots (project_id TEXT, path TEXT);
                 CREATE TABLE thread_sections (id TEXT PRIMARY KEY);
                 CREATE TABLE threads (id TEXT PRIMARY KEY, project_id TEXT, thread_section_id TEXT);
                 CREATE TABLE thread_dynamic_tools (thread_id TEXT, name TEXT);
                 CREATE TABLE thread_spawn_edges (parent_thread_id TEXT, child_thread_id TEXT);
                 CREATE TABLE _sqlx_migrations (version INTEGER);
                 INSERT INTO projects VALUES ('kept-project'), ('other-project');
                 INSERT INTO project_roots VALUES ('kept-project', '/kept'), ('other-project', '/other');
                 INSERT INTO thread_sections VALUES ('kept-section'), ('other-section');
                 INSERT INTO threads VALUES ('kept', 'kept-project', 'kept-section'), ('other', 'other-project', 'other-section');
                 INSERT INTO thread_dynamic_tools VALUES ('kept', 'kept-tool'), ('other', 'other-tool');
                 INSERT INTO thread_spawn_edges VALUES ('kept', 'other');"
            ).unwrap();
            for (table, type_column) in &tables {
                create_attachment_table(&conn, "main", table, type_column);
                conn.execute_batch(&format!(
                    "INSERT INTO {table} VALUES
                        ('kept-{table}', 'kept', 'pull_request', 'kept-key', '{{\"url\":\"kept\"}}', 123),
                        ('other-{table}', 'other', 'pull_request', 'other-key', '{{\"url\":\"other\"}}', 456);"
                )).unwrap();
            }
            drop(conn);
            trim_export_database(&database.path, &["kept".into()], &HashSet::new()).unwrap();
            let trimmed = Connection::open(&database.path).unwrap();
            for (table, _) in &tables {
                let rows = trimmed.prepare(&format!("SELECT thread_id, payload FROM {table}")).unwrap()
                    .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                    .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(rows, vec![("kept".into(), "{\"url\":\"kept\"}".into())]);
            }
            for (table, column, expected) in [
                ("threads", "id", "kept"), ("projects", "id", "kept-project"),
                ("project_roots", "project_id", "kept-project"), ("thread_sections", "id", "kept-section"),
                ("thread_dynamic_tools", "thread_id", "kept"),
            ] {
                let values = trimmed.prepare(&format!("SELECT {column} FROM {table}")).unwrap()
                    .query_map([], |row| row.get::<_, String>(0)).unwrap()
                    .collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(values, vec![expected], "{table}");
            }
            assert_eq!(trimmed.query_row("SELECT COUNT(*) FROM thread_spawn_edges", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
            assert!(!database_table_exists(&trimmed, "main", "_sqlx_migrations").unwrap());
            assert_eq!(trimmed.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0)).unwrap(), "ok");
        }
    }

    #[test]
    fn copies_attachments_across_versions_for_import_and_delete_restore() {
        for (schema, scope) in [("imported_package", "importing_thread_ids"), ("deleted_backup", "restored_thread_ids")] {
            for (source_table, source_type) in THREAD_ATTACHMENT_TABLES {
                for (target_table, target_type) in THREAD_ATTACHMENT_TABLES {
                    let mut conn = Connection::open_in_memory().unwrap();
                    conn.execute_batch(&format!(
                        "PRAGMA foreign_keys = ON;
                         ATTACH DATABASE ':memory:' AS {schema};
                         CREATE TABLE main.threads (id TEXT PRIMARY KEY, title TEXT, creator_user_id TEXT, history_mode TEXT NOT NULL DEFAULT 'legacy');
                         CREATE TABLE {schema}.threads (title TEXT, id TEXT PRIMARY KEY);
                         INSERT INTO {schema}.threads VALUES ('Kept title', 'kept'), ('Other title', 'other');
                         CREATE TEMP TABLE {scope} (id TEXT PRIMARY KEY);
                         INSERT INTO {scope} VALUES ('kept');"
                    )).unwrap();
                    create_attachment_table(&conn, schema, source_table, source_type);
                    create_attachment_table(&conn, "main", target_table, target_type);
                    conn.execute_batch(&format!(
                        "INSERT INTO {schema}.{source_table} VALUES
                            ('kept-attachment', 'kept', 'pull_request', 'key', '{{\"url\":\"kept\"}}', 123),
                            ('other-attachment', 'other', 'worktree', 'other-key', '{{\"root\":\"other\"}}', 456);"
                    )).unwrap();
                    let transaction = conn.transaction().unwrap();
                    copy_database_rows(&transaction, schema, "threads", &format!("id IN (SELECT id FROM {scope})"), false).unwrap();
                    copy_thread_attachments(&transaction, schema, scope).unwrap();
                    transaction.commit().unwrap();
                    let thread: (String, String, Option<String>, String) = conn.query_row(
                        "SELECT id, title, creator_user_id, history_mode FROM main.threads", [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    ).unwrap();
                    assert_eq!(thread, ("kept".into(), "Kept title".into(), None, "legacy".into()));
                    let mut statement = conn.prepare(&format!("SELECT id, thread_id, {target_type}, identity_key, payload, created_at FROM main.{target_table}")).unwrap();
                    let rows = statement.query_map([], |row| Ok((
                        row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, i64>(5)?,
                    ))).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
                    assert_eq!(rows, vec![("kept-attachment".into(), "kept".into(), "pull_request".into(), "key".into(), "{\"url\":\"kept\"}".into(), 123)]);
                    assert!(!database_table_exists(&conn, "main", if target_table == "thread_artifacts" { "thread_attachments" } else { "thread_artifacts" }).unwrap());
                }
            }
        }
    }

    #[test]
    fn allows_attachment_free_packages_and_unselected_attachments() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "ATTACH DATABASE ':memory:' AS imported_package;
             CREATE TABLE imported_package.threads (id TEXT PRIMARY KEY);
             INSERT INTO imported_package.threads VALUES ('other');
             CREATE TEMP TABLE importing_thread_ids (id TEXT PRIMARY KEY);
             INSERT INTO importing_thread_ids VALUES ('kept');"
        ).unwrap();
        copy_thread_attachments(&conn, "imported_package", "importing_thread_ids").unwrap();
        create_attachment_table(&conn, "imported_package", "thread_attachments", "attachment_type");
        conn.execute_batch("INSERT INTO imported_package.thread_attachments VALUES ('other-attachment', 'other', 'worktree', 'key', '{}', 123)").unwrap();
        copy_thread_attachments(&conn, "imported_package", "importing_thread_ids").unwrap();
        create_attachment_table(&conn, "main", "thread_attachments", "attachment_type");
        conn.execute_batch("DROP TABLE imported_package.thread_attachments").unwrap();
        copy_thread_attachments(&conn, "imported_package", "importing_thread_ids").unwrap();
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM thread_attachments", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn rejects_attachment_loss_and_rolls_back_copied_threads() {
        for (schema, scope) in [("imported_package", "importing_thread_ids"), ("deleted_backup", "restored_thread_ids")] {
            let mut conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "ATTACH DATABASE ':memory:' AS {schema};
                 CREATE TABLE main.threads (id TEXT PRIMARY KEY);
                 CREATE TABLE {schema}.threads (id TEXT PRIMARY KEY);
                 INSERT INTO {schema}.threads VALUES ('kept');
                 CREATE TEMP TABLE {scope} (id TEXT PRIMARY KEY);
                 INSERT INTO {scope} VALUES ('kept');"
            )).unwrap();
            create_attachment_table(&conn, schema, "thread_artifacts", "artifact_type");
            conn.execute_batch(&format!("INSERT INTO {schema}.thread_artifacts VALUES ('attachment', 'kept', 'worktree', 'key', '{{}}', 123)")).unwrap();
            {
                let transaction = conn.transaction().unwrap();
                copy_database_rows(&transaction, schema, "threads", &format!("id IN (SELECT id FROM {scope})"), false).unwrap();
                let error = copy_thread_attachments(&transaction, schema, scope).unwrap_err();
                assert!(error.contains("不支持会话附件"), "{error}");
            }
            assert_eq!(conn.query_row("SELECT COUNT(*) FROM main.threads", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        }
    }

    #[test]
    fn copying_rows_keeps_named_values_and_existing_project_conflicts() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "ATTACH DATABASE ':memory:' AS imported_package;
             CREATE TABLE main.projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, position INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE imported_package.projects (name TEXT NOT NULL, id TEXT PRIMARY KEY, newer_field TEXT);
             INSERT INTO main.projects VALUES ('existing', 'Local name', 5);
             INSERT INTO imported_package.projects VALUES ('Exported name', 'existing', 'metadata'), ('New name', 'new', 'metadata'), ('Other name', 'other', 'metadata');"
        ).unwrap();
        copy_database_rows(&conn, "imported_package", "projects", "id IN ('existing', 'new')", true).unwrap();
        let rows = conn.prepare("SELECT id, name, position FROM main.projects ORDER BY id").unwrap()
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)))
            .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows, vec![("existing".into(), "Local name".into(), 5), ("new".into(), "New name".into(), 0)]);
    }

    #[test]
    fn copying_rows_rejects_missing_tables_and_required_fields() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "ATTACH DATABASE ':memory:' AS imported_package;
             CREATE TABLE main.projects (id TEXT PRIMARY KEY, required_field TEXT NOT NULL);
             CREATE TABLE imported_package.projects (id TEXT PRIMARY KEY);
             INSERT INTO imported_package.projects VALUES ('project');"
        ).unwrap();
        assert!(copy_database_rows(&conn, "imported_package", "projects", "1", true).unwrap_err().contains("required_field"));
        assert!(copy_database_rows(&conn, "imported_package", "threads", "1", false).unwrap_err().contains("缺少 threads 表"));
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM main.projects", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    #[ignore = "requires a populated local Codex database; run explicitly with --ignored"]
    fn audit_counts_only_sidebar_visible_sessions() {
        let home = codex_home().expect("Codex home should exist for the local integration test");
        let database = state_database(&home).expect("Codex state database should exist");
        let conn = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("state database should be readable");
        let projects = query_projects(&conn).expect("projects should be queryable");
        let assignments = load_desktop_project_assignments(&home);
        let visible_threads = query_threads(&conn).expect("threads should be queryable")
            .into_iter().filter(|thread| thread.source == "vscode").collect::<Vec<_>>();
        let audit = report().expect("audit should scan the local Codex store");
        assert_eq!(audit.sessions.len(), visible_threads.len());

        for project in &audit.projects {
            let expected_for_root = visible_threads.iter()
                .filter(|thread| effective_project_id(thread, &projects, &assignments).as_deref() == Some(&project.id))
                .count();
            assert_eq!(project.session_count, expected_for_root, "{}", project.name);
        }
    }

    #[test]
    fn audit_keeps_sessions_with_their_project_after_a_root_change() {
        let database = TemporaryDatabase::new();
        let home = &database.folder;
        fs::create_dir(home.join("sessions")).unwrap();
        let log = home.join("sessions/session.jsonl");
        fs::write(&log, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"migrated\",\"cwd\":\"/old-root\"}}\n").unwrap();
        let conn = Connection::open(&database.path).unwrap();
        conn.execute_batch("CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT,position INTEGER); CREATE TABLE project_roots(project_id TEXT,position INTEGER,path TEXT);
            CREATE TABLE threads(id TEXT PRIMARY KEY,name TEXT,title TEXT,source TEXT,cwd TEXT,archived INTEGER,rollout_path TEXT,project_id TEXT,recency_at_ms INTEGER);
            INSERT INTO projects VALUES('current-project','Migrated project',0); INSERT INTO project_roots VALUES('current-project',0,'/new-root')").unwrap();
        conn.execute("INSERT INTO threads VALUES('migrated',NULL,'Session','vscode','/old-root',0,?1,NULL,1)", params![display_path(&log)]).unwrap();
        fs::write(home.join(".codex-global-state.json"), serde_json::json!({
            "thread-project-assignments":{"migrated":{"projectKind":"local","projectId":"legacy-project"}},
            "app-server-project-id-by-legacy-project-id-by-host":{"local":{"legacy-project":"current-project"}}
        }).to_string()).unwrap();
        let audit = report_at(home).unwrap();
        let audited = audit.sessions.iter().find(|session| session.id == "migrated").unwrap();
        assert_eq!(audited.project_id.as_deref(), Some("current-project"));
        assert_eq!(audited.status, "mismatch");
        assert_eq!(audit.projects[0].session_count, 1);
        assert_eq!(audit.projects[0].issue_count, 1);
    }

    #[test]
    #[ignore = "requires a populated local Codex database with a named sidebar session"]
    fn audit_uses_the_codex_sidebar_name_when_present() {
        let home = codex_home().expect("Codex home should exist for the local integration test");
        let database = state_database(&home).expect("Codex state database should exist");
        let conn = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("state database should be readable");
        let (id, sidebar_name): (String, String) = conn.query_row(
            "SELECT id, name FROM threads WHERE source = 'vscode' AND TRIM(COALESCE(name, '')) <> '' LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).expect("the local Codex database should contain a named sidebar session");
        let audited_title = query_threads(&conn).expect("threads should be queryable")
            .into_iter().find(|thread| thread.id == id)
            .expect("the named sidebar session should be present").title;
        assert_eq!(audited_title, sidebar_name);
    }

    #[test]
    fn identifies_only_codex_processes_that_can_update_local_state() {
        assert!(is_codex_process("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"));
        assert!(is_codex_process("/Applications/ChatGPT.app/Contents/Resources/codex app-server"));
        assert!(is_codex_process("Codex.exe"));
        assert!(is_codex_process("codex-code-mode-host.exe"));
        assert!(is_codex_process(r"C:\Program Files\OpenAI\ChatGPT.exe"));
        assert!(!is_codex_process("target/debug/codex-session-manager"));
        assert!(!is_codex_process("codex-session-manager.exe"));
        assert!(!is_codex_process("node /workspace/node_modules/.bin/vite"));
    }

    #[test]
    fn parses_only_matching_codex_processes() {
        let line = "  123 /Applications/ChatGPT.app/Contents/MacOS/ChatGPT";
        let mut fields = line.trim().splitn(2, char::is_whitespace);
        assert_eq!(fields.next(), Some("123"));
        assert!(is_codex_process(fields.next().unwrap()));
    }

    #[test]
    fn parses_windows_tasklist_csv_without_localized_headers() {
        let process = parse_windows_tasklist_line(
            r#""Codex.exe","4242","Console","1","81,920 K""#,
        ).expect("tasklist row should parse");
        assert_eq!(process, ("4242".into(), "Codex.exe".into()));
        assert!(parse_windows_tasklist_line("INFO: No tasks are running").is_none());
    }

    #[test]
    fn windows_path_comparison_handles_separators_case_and_device_prefixes() {
        assert_eq!(
            normalized_for_platform(r"C:\Users\Knight\Project\", true),
            "c:/users/knight/project"
        );
        assert_eq!(
            normalized_for_platform(r"\\?\C:\Users\KNIGHT\Project", true),
            "c:/users/knight/project"
        );
        assert_eq!(normalized_for_platform(r"C:\", true), "c:/");
        assert_ne!(
            normalized_for_platform("/Users/Knight/Project", false),
            normalized_for_platform("/users/knight/project", false)
        );
    }

    #[test]
    fn windows_display_paths_hide_device_prefixes() {
        assert_eq!(
            display_path_for_platform(r"\\?\E:\Workspace\Project".into(), true),
            r"E:\Workspace\Project"
        );
        assert_eq!(
            display_path_for_platform(r"\\?\UNC\server\share\Project".into(), true),
            r"\\server\share\Project"
        );
    }

    #[test]
    fn imported_rollout_paths_use_codex_standard_filenames() {
        let home = Path::new(r"C:\Users\tester\.codex");
        let path = imported_log_destination(home, "batch", "01a08167-7986-71d2-a018-09e71af7734f", 0);
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("rollout-"));
        assert!(name.ends_with("-01a08167-7986-71d2-a018-09e71af7734f.jsonl"));
        assert!(path.starts_with(home.join("sessions").join("imported").join("batch")));
    }

    #[test]
    fn atomic_write_replaces_an_existing_file() {
        let folder = std::env::temp_dir().join(format!(
            "codex-session-manager-atomic-write-{}-{}",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&folder).expect("temporary folder should be created");
        let path = folder.join("session.jsonl");
        fs::write(&path, "old\n").expect("old file should be written");
        atomic_write(&path, "new\n").expect("existing file should be replaced");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        fs::remove_dir_all(folder).expect("temporary folder should be removed");
    }

    #[test]
    fn purges_only_the_deleted_thread_references_from_global_state() {
        let mut state = serde_json::json!({
            "thread-project-assignments": { "deleted": { "projectId": "project-a" }, "kept": { "projectId": "project-b" } },
            "pinned-thread-ids": ["deleted", "kept"],
            "new-thread:client-a": "deleted",
            "client-new-thread:client-b": "deleted",
            "thread-reference-capability:deleted": true,
            "unrelated": "deleted"
        });
        assert_eq!(purge_thread_reference(&mut state, "deleted"), 5);
        assert!(state["thread-project-assignments"].get("deleted").is_none());
        assert_eq!(state["thread-project-assignments"]["kept"]["projectId"], "project-b");
        assert_eq!(state["pinned-thread-ids"], serde_json::json!(["kept"]));
        assert!(state.get("new-thread:client-a").is_none());
        assert!(state.get("client-new-thread:client-b").is_none());
        assert!(state.get("thread-reference-capability:deleted").is_none());
        assert_eq!(state["unrelated"], "deleted");
    }

    #[test]
    fn migration_updates_only_cwd_fields_and_keeps_file_links() {
        let old = "/Users/knight/Desktop/Test";
        let target = "/Users/knight/Desktop/Test2";
        let database = TemporaryDatabase::new();
        let log = database.folder.join("session.jsonl");
        let message = "{\"type\":\"response_item\",\"payload\":{\"cwd\":\"/Users/knight/Desktop/Test\",\"message\":\"[Test1](</Users/knight/Desktop/Test/Test1.md>)\"}}\n";
        fs::write(&log, format!("{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{old}\"}}}}\n{message}")).unwrap();
        let rewrite = prepare_log_rewrite(&log, &HashSet::from([old.to_string()]), target).unwrap();
        let rewritten = rewrite.contents;
        assert_eq!(rewrite.replacements, 1);
        assert!(rewritten.contains("\"cwd\":\"/Users/knight/Desktop/Test2\""));
        assert!(rewritten.contains("/Users/knight/Desktop/Test/Test1.md"));
        assert!(rewritten.ends_with(message));
    }

    #[test]
    fn repair_history_manifest_round_trips() {
        let folder = std::env::temp_dir().join(format!(
            "codex-session-manager-history-{}-{}",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&folder).expect("temporary history folder should be created");
        let manifest = RepairManifest {
            version: 2,
            created_at: "2026-09-05T00:00:00+08:00".into(),
            thread_id: "thread-test".into(),
            session_title: "测试会话".into(),
            source_cwd: "/old".into(),
            target_cwd: "/new".into(),
            files: vec![BackupFileEntry { original_path: "/original".into(), backup_name: "backup".into() }],
            thread_changes: vec![ThreadRepairChange {
                thread_id: "child-test".into(),
                session_title: "子代理".into(),
                source_cwd: "/old-child".into(),
                target_cwd: "/new".into(),
                sandbox_policy: None,
                desktop_path_settings: HashMap::new(),
            }],
            rolled_back_at: None,
        };
        let path = write_manifest(&folder, &manifest).expect("manifest should be written");
        let restored = read_manifest(&path).expect("manifest should be readable");
        assert_eq!(restored.thread_id, manifest.thread_id);
        assert_eq!(restored.source_cwd, "/old");
        assert_eq!(restored.files.len(), 1);
        assert_eq!(restored.thread_changes.len(), 1);
        fs::remove_dir_all(folder).expect("temporary history folder should be removed");
    }

    #[test]
    fn delete_history_lists_only_completed_operations() {
        let base = std::env::temp_dir().join(format!(
            "codex-session-manager-delete-history-{}-{}",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let completed_folder = base.join("completed-deletion");
        let incomplete_folder = base.join("incomplete-deletion");
        fs::create_dir_all(&completed_folder).expect("completed deletion folder should be created");
        fs::create_dir_all(&incomplete_folder).expect("incomplete deletion folder should be created");
        let manifest = DeleteManifest {
            version: 1,
            created_at: "2026-09-05T00:00:00+08:00".into(),
            completed_at: Some("2026-09-05T00:00:01+08:00".into()),
            deletion_kind: "session".into(),
            thread_id: Some("thread-test".into()),
            session_title: "测试删除会话".into(),
            source_path: None,
            child_count: 2,
            files: vec![BackupFileEntry { original_path: "/original".into(), backup_name: "backup".into() }],
            rolled_back_at: None,
        };
        write_delete_manifest(&completed_folder, &manifest).expect("completed manifest should be written");
        let mut incomplete = manifest.clone();
        incomplete.completed_at = None;
        write_delete_manifest(&incomplete_folder, &incomplete).expect("incomplete manifest should be written");

        let items = delete_history(&base, Some(base.to_str().expect("temporary path should be UTF-8")))
            .expect("delete history should load");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].thread_id.as_deref(), Some("thread-test"));
        assert_eq!(items[0].child_count, 2);
        assert_eq!(items[0].file_count, 1);
        fs::remove_dir_all(base).expect("temporary history folder should be removed");
    }

    #[test]
    fn legacy_repair_manifest_remains_readable() {
        let legacy = r#"{
          "version": 1,
          "createdAt": "2026-09-05T00:00:00+08:00",
          "threadId": "thread-test",
          "sessionTitle": "测试会话",
          "sourceCwd": "/old",
          "targetCwd": "/new",
          "files": [],
          "rolledBackAt": null
        }"#;
        let manifest: RepairManifest = serde_json::from_str(legacy).expect("legacy manifest should deserialize");
        assert_eq!(manifest.version, 1);
        assert!(manifest.thread_changes.is_empty());
    }

    #[test]
    fn finds_all_descendant_agents_once() {
        let conn = Connection::open_in_memory().expect("in-memory database should open");
        conn.execute_batch(
            "CREATE TABLE thread_spawn_edges (
                parent_thread_id TEXT NOT NULL,
                child_thread_id TEXT NOT NULL PRIMARY KEY,
                status TEXT NOT NULL
            );
            INSERT INTO thread_spawn_edges VALUES
                ('root', 'child-a', 'completed'),
                ('root', 'child-b', 'completed'),
                ('child-a', 'grandchild', 'completed');",
        ).expect("test edges should be created");
        assert_eq!(descendant_thread_ids(&conn, "root").expect("descendants should load"), vec!["child-a", "child-b", "grandchild"]);
    }

    #[test]
    fn parses_successful_thread_delete_response() {
        let output = r#"{"id":1,"result":{"userAgent":"Codex"}}
{"id":2,"result":{}}
{"method":"thread/deleted","params":{"threadId":"thread-test"}}"#;
        assert!(parse_thread_delete_response(output).is_ok());
    }

    #[test]
    fn returns_thread_delete_error_message() {
        let output = r#"{"error":{"code":-32603,"message":"thread is active"},"id":2}"#;
        assert_eq!(parse_thread_delete_response(output).unwrap_err(), "thread is active");
    }

    #[test]
    #[ignore = "requires a populated local Codex database; run explicitly with --ignored"]
    fn trimmed_export_snapshot_keeps_only_the_requested_thread() {
        let home = codex_home().expect("Codex home should exist for the local integration test");
        let database = state_database(&home).expect("state database should exist");
        let source = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("state database should be readable");
        let thread_id: String = source.query_row(
            "SELECT id FROM threads WHERE source = 'vscode' ORDER BY recency_at_ms DESC LIMIT 1",
            [],
            |row| row.get(0),
        ).expect("a visible Codex session should exist");
        let temporary = std::env::temp_dir().join(format!(
            "codex-session-manager-export-test-{}-{}",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&temporary).expect("temporary export folder should be created");
        backup_database(&database, &temporary).expect("snapshot should be created");
        trim_export_database(&temporary.join("state_5.sqlite"), std::slice::from_ref(&thread_id), &HashSet::new())
            .expect("snapshot should be trimmed");
        let trimmed = Connection::open(temporary.join("state_5.sqlite")).expect("trimmed snapshot should open");
        let count: i64 = trimmed.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .expect("trimmed thread count should load");
        let kept: String = trimmed.query_row("SELECT id FROM threads", [], |row| row.get(0))
            .expect("requested thread should remain");
        assert_eq!(count, 1);
        assert_eq!(kept, thread_id);
        fs::remove_dir_all(temporary).expect("temporary export folder should be removed");
    }

    #[test]
    fn resolves_desktop_project_ids_for_project_wide_actions() {
        let state = serde_json::json!({
            "local-projects": { "local-a": { "name": "A" } },
            "app-server-project-id-by-legacy-project-id-by-host": {
                "local:C:/Users/test/.codex": { "local-a": "project-a", "local-b": "project-b" },
                "local:/Users/test/.codex": { "mac-a": "project-a" }
            }
        });
        assert_eq!(
            desktop_legacy_project_ids(&state, "project-a"),
            HashSet::from(["local-a".to_string(), "mac-a".to_string()])
        );
        assert!(desktop_legacy_project_ids(&state, "missing").is_empty());
    }

    #[test]
    fn import_plan_registers_a_local_project_and_uses_its_desktop_id_for_assignments() {
        let temporary = std::env::temp_dir().join(format!(
            "codex-session-manager-import-plan-{}-{}",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let home = temporary.join("codex-home");
        let target = temporary.join("workspace");
        fs::create_dir_all(&home).expect("temporary Codex home should be created");
        fs::create_dir_all(&target).expect("temporary workspace should be created");
        let source_project_id = "01a00000-test-project".to_string();
        let projects = vec![ExportProject {
            id: source_project_id.clone(),
            name: "Migrated project".into(),
            roots: vec!["/Users/test/source".into()],
        }];
        let mappings = vec![ImportProjectMapping {
            project_id: source_project_id.clone(),
            target_path: display_path(&target),
        }];
        let plan = prepare_desktop_import_plan(&home, &projects, &mappings)
            .expect("desktop import plan should be prepared");
        let resolved = plan.projects.get(&source_project_id).unwrap().clone();
        assert_eq!(resolved.target_path, display_path(&target));
        assert_eq!(resolved.app_server_project_id, source_project_id);
        assert!(resolved.desktop_project_id.starts_with("local-"));

        let assignments = HashMap::from([("thread-test".into(), Some(source_project_id))]);
        let mut plan = plan;
        assert_eq!(update_desktop_import_state(&mut plan, &assignments).unwrap(), 1);
        atomic_write(&plan.state_path, &plan.state.to_string()).unwrap();
        let state: Value = serde_json::from_slice(&fs::read(home.join(".codex-global-state.json")).unwrap()).unwrap();
        assert_eq!(
            state["thread-project-assignments"]["thread-test"]["projectId"].as_str(),
            Some(resolved.desktop_project_id.as_str())
        );
        assert_eq!(
            state["local-projects"][&resolved.desktop_project_id]["rootPaths"][0].as_str(),
            Some(display_path(&target).as_str())
        );
        fs::remove_dir_all(temporary).expect("temporary import plan folder should be removed");
    }
}
