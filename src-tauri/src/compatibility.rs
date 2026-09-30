use super::*;

#[cfg(test)]
#[path = "compatibility_tests.rs"]
mod tests;

pub(super) const HISTORY_FILE: &str = "thread_history_1.sqlite";
pub(super) const THREAD_PATH_KEYS: [&str; 3] = [
    "thread-writable-roots", "thread-workspace-root-hints", "thread-projectless-output-directories",
];
const METADATA_PATH_KEYS: [&str; 7] = [
    "cwd", "runtime_workspace_roots", "workspace_roots", "sandbox_policy",
    "file_system_sandbox_policy", "permission_profile", "writable_roots",
];

pub(super) fn history_database(home: &Path) -> PathBuf {
    let nested = home.join("sqlite").join(HISTORY_FILE);
    if !home.join(HISTORY_FILE).exists() && nested.is_file() { nested } else { home.join(HISTORY_FILE) }
}

pub(super) fn package_history_path(package: &Path, manifest: &SessionExportManifest) -> Result<PathBuf, String> {
    if let Some(name) = &manifest.history_database {
        if name != HISTORY_FILE { return Err("导出包中的分页历史文件名无效。".into()); }
        let path = safe_package_file(package, name)?;
        if !path.is_file() { return Err("导出包缺少清单声明的分页历史数据库。".into()); }
        Ok(path)
    } else { Ok(package.join(HISTORY_FILE)) }
}

pub(super) fn snapshot_database(source: &Path, destination: &Path) -> Result<(), String> {
    let conn = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("无法打开要备份的数据库：{e}"))?;
    conn.backup(DatabaseName::Main, destination, None)
        .map_err(|e| format!("创建一致性数据库快照失败：{e}"))
}

fn history_tables(conn: &Connection, schema: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn.prepare(&format!(
        "SELECT name FROM {}.sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name != '_sqlx_migrations' ORDER BY name",
        quote_sql_identifier(schema),
    )).map_err(|e| e.to_string())?;
    let tables = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    for table in &tables {
        if !database_table_columns(conn, schema, table)?.iter().any(|column| column == "thread_id") {
            return Err(format!("无法识别新版历史表 {table} 的会话范围，已取消操作。"));
        }
    }
    Ok(tables)
}

fn sqlite_read_only_uri(path: &Path) -> Result<String, String> {
    let path = path.to_str().ok_or("数据库路径不是有效的 UTF-8，无法创建只读 URI。")?;
    // SQLite decodes the path before passing it to the filesystem. Encode separators
    // too so Windows verbatim/UNC prefixes cannot become a URI authority, while
    // preserving the original path (including long-path prefixes and Unix backslashes).
    let encoded = path.bytes()
        .map(|byte| if byte.is_ascii_alphanumeric() || b".-_~".contains(&byte) { (byte as char).to_string() } else { format!("%{byte:02X}") })
        .collect::<String>();
    Ok(format!("file:{encoded}?mode=ro"))
}

pub(super) fn snapshot_history(home: &Path, destination: &Path, ids: &[String]) -> Result<Option<BackupFileEntry>, String> {
    let source = history_database(home);
    if !source.is_file() { return Ok(None); }
    let snapshot = destination.join(HISTORY_FILE);
    if snapshot.exists() { return Err("备份目录中已存在分页历史快照，已拒绝覆盖。".into()); }
    let source_conn = Connection::open_with_flags(&source, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
    let tables = history_tables(&source_conn, "main")?;
    let schemas = source_conn.prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND sql IS NOT NULL ORDER BY name").map_err(|e| e.to_string())?
        .query_map([], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    let indexes = source_conn.prepare("SELECT sql FROM sqlite_master WHERE type='index' AND sql IS NOT NULL ORDER BY name").map_err(|e| e.to_string())?
        .query_map([], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    // Select rows directly into a fresh database; unrelated payloads never enter the backup.
    let partial = destination.join(".thread-history-incomplete.sqlite");
    let result = (|| -> Result<(), String> {
        let mut conn = Connection::open(&partial).map_err(|e| e.to_string())?;
        for sql in schemas { conn.execute_batch(&sql).map_err(|e| e.to_string())?; }
        let source_path = fs::canonicalize(&source).map_err(|e| e.to_string())?;
        conn.execute("ATTACH DATABASE ?1 AS source_history", params![sqlite_read_only_uri(&source_path)?]).map_err(|e| e.to_string())?;
        conn.execute_batch("CREATE TEMP TABLE scoped_history_threads (id TEXT PRIMARY KEY)").map_err(|e| e.to_string())?;
        let transaction = conn.transaction().map_err(|e| e.to_string())?;
        for id in ids { transaction.execute("INSERT OR IGNORE INTO scoped_history_threads VALUES (?1)", params![id]).map_err(|e| e.to_string())?; }
        for table in tables {
            copy_database_rows(&transaction, "source_history", &table, "thread_id IN (SELECT id FROM scoped_history_threads)", false)?;
        }
        if database_table_exists(&transaction, "source_history", "_sqlx_migrations")? {
            copy_database_rows(&transaction, "source_history", "_sqlx_migrations", "1", false)?;
        }
        for sql in indexes { transaction.execute_batch(&sql).map_err(|e| e.to_string())?; }
        transaction.commit().map_err(|e| e.to_string())?;
        drop(conn);
        fs::rename(&partial, &snapshot).map_err(|e| e.to_string())
    })();
    if let Err(error) = result { let _ = fs::remove_file(&partial); return Err(error); }
    Ok(Some(BackupFileEntry { original_path: display_path(&source), backup_name: HISTORY_FILE.into() }))
}

pub(super) fn paginated_threads(conn: &Connection, schema: &str, ids: &[String]) -> Result<Vec<String>, String> {
    if !database_table_columns(conn, schema, "threads")?.iter().any(|column| column == "history_mode") { return Ok(Vec::new()); }
    let mut result = Vec::new();
    for id in ids {
        let mode: String = conn.query_row(&format!("SELECT history_mode FROM {}.threads WHERE id = ?1", quote_sql_identifier(schema)), params![id], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        match mode.as_str() {
            "paginated" => result.push(id.clone()),
            "legacy" => {},
            _ => return Err(format!("尚不支持 Codex 历史模式 {mode}，已取消操作。")),
        }
    }
    Ok(result)
}

pub(super) fn validate_history(conn: &Connection, schema: &str, history: &Path, ids: &[String]) -> Result<(), String> {
    let paginated = paginated_threads(conn, schema, ids)?;
    if paginated.is_empty() { return Ok(()); }
    if !history.is_file() {
        return Err("这些会话使用新版分页历史，但导出包或备份缺少 thread_history_1.sqlite；请用新版管理器重新导出或选择完整备份。".into());
    }
    let history = Connection::open_with_flags(history, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
    history_tables(&history, "main")?;
    if !database_table_exists(&history, "main", "thread_history_projection_state")? {
        return Err("分页历史数据库缺少日志索引，已取消操作。".into());
    }
    for id in paginated {
        let found: bool = history.query_row("SELECT EXISTS(SELECT 1 FROM thread_history_projection_state WHERE thread_id = ?1)", params![id], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        if !found { return Err(format!("分页历史中缺少会话 {id} 的日志索引，已取消操作以免恢复为空白会话。")); }
    }
    Ok(())
}

// Attach to the state connection so SQL errors roll back metadata and history together.
pub(super) fn attach_history(conn: &Connection, home: &Path, incoming: Option<&Path>) -> Result<bool, String> {
    let destination = history_database(home);
    if let Some(source) = incoming.filter(|path| path.is_file()) {
        if !destination.exists() {
            snapshot_database(source, &destination)?;
            let empty = Connection::open(&destination).map_err(|e| e.to_string())?;
            for table in history_tables(&empty, "main")? {
                empty.execute(&format!("DELETE FROM {}", quote_sql_identifier(&table)), []).map_err(|e| e.to_string())?;
            }
        }
        conn.execute("ATTACH DATABASE ?1 AS incoming_history", params![source.to_string_lossy().as_ref()]).map_err(|e| e.to_string())?;
    }
    if !destination.is_file() { return Ok(false); }
    conn.execute("ATTACH DATABASE ?1 AS thread_history", params![destination.to_string_lossy().as_ref()]).map_err(|e| e.to_string())?;
    history_tables(conn, "thread_history")?;
    Ok(true)
}

pub(super) fn copy_history(conn: &Connection, scope: &str) -> Result<(), String> {
    for table in history_tables(conn, "thread_history")? {
        conn.execute(&format!("DELETE FROM thread_history.{} WHERE thread_id IN (SELECT id FROM {})", quote_sql_identifier(&table), quote_sql_identifier(scope)), []).map_err(|e| e.to_string())?;
    }
    for table in history_tables(conn, "incoming_history")? {
        let source_columns = database_table_columns(conn, "incoming_history", &table)?;
        let target_columns = database_table_columns(conn, "thread_history", &table)?;
        if source_columns.iter().any(|column| !target_columns.contains(column)) {
            return Err(format!("当前 Codex 的历史表 {table} 缺少源版本字段，请升级 Codex 后重试。"));
        }
        copy_database_rows_into(conn, "incoming_history", "thread_history", &table,
            &format!("thread_id IN (SELECT id FROM {})", quote_sql_identifier(scope)), false)?;
    }
    Ok(())
}

// Paths are changed only in structured runtime metadata, never in chat/tool text.
pub(super) fn mapped_path(path: &str, previous: &HashSet<String>, target: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let mut roots = previous.iter().collect::<Vec<_>>();
    roots.sort_by_key(|root| std::cmp::Reverse(root.len()));
    for root in roots {
        let root = root.replace('\\', "/");
        let root = root.trim_end_matches('/');
        if root.is_empty() { continue; }
        let windows = root.as_bytes().get(1) == Some(&b':') || root.starts_with("//");
        let matches = if windows { normalized.get(..root.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(root)) } else { normalized.starts_with(root) };
        if matches && (normalized.len() == root.len() || normalized.as_bytes().get(root.len()) == Some(&b'/')) {
            let suffix = &normalized[root.len()..];
            let mapped = format!("{}{suffix}", target.trim_end_matches(['/', '\\']));
            if mapped != path { return Some(mapped); }
        }
    }
    None
}

pub(super) fn remap_paths(value: &mut Value, previous: &HashSet<String>, target: &str) -> usize {
    match value {
        Value::String(path) => if let Some(mapped) = mapped_path(path, previous, target) { *path = mapped; 1 } else { 0 },
        Value::Array(items) => items.iter_mut().map(|item| remap_paths(item, previous, target)).sum(),
        Value::Object(fields) => fields.values_mut().map(|item| remap_paths(item, previous, target)).sum(),
        _ => 0,
    }
}

fn remap_filesystem_xml(xml: &str, previous: &HashSet<String>, target: &str) -> (String, usize) {
    let mut result = xml.to_string();
    let mut replacements = 0;
    for tag in ["root", "path"] {
        let open = format!("<{tag}>"); let close = format!("</{tag}>");
        let mut cursor = 0;
        while let Some(start) = result[cursor..].find(&open).map(|offset| cursor + offset + open.len()) {
            let Some(end) = result[start..].find(&close).map(|offset| start + offset) else { break };
            let decoded = result[start..end].replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&");
            if let Some(mapped) = mapped_path(&decoded, previous, target) {
                let encoded = mapped.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;");
                result.replace_range(start..end, &encoded);
                cursor = start + encoded.len() + close.len(); replacements += 1;
            } else { cursor = end + close.len(); }
        }
    }
    (result, replacements)
}

fn rewrite_metadata(event: &mut Value, previous: &HashSet<String>, target: &str) -> usize {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    let Some(payload) = event.get_mut("payload") else { return 0 };
    let mut count = 0;
    if matches!(kind.as_str(), "session_meta" | "turn_context") {
        for key in METADATA_PATH_KEYS {
            if let Some(value) = payload.get_mut(key) { count += remap_paths(value, previous, target); }
        }
    } else if kind == "world_state" {
        if let Some(environments) = payload.pointer_mut("/state/environments") {
            if let Some(items) = environments.get_mut("environments").and_then(Value::as_object_mut) {
                for environment in items.values_mut() {
                    if let Some(cwd) = environment.get_mut("cwd") { count += remap_paths(cwd, previous, target); }
                }
            }
            if let Some(Value::String(xml)) = environments.get_mut("filesystem") {
                let (updated, replaced) = remap_filesystem_xml(xml, previous, target);
                *xml = updated; count += replaced;
            }
        }
    }
    count
}

#[derive(Debug)]
pub(super) struct LogRewrite {
    pub path: PathBuf,
    pub original: String,
    pub existed: bool,
    pub contents: String,
    pub replacements: usize,
    pub offsets: HashMap<i64, i64>,
}

pub(super) fn prepare_log_rewrite(path: &Path, previous: &HashSet<String>, target: &str) -> Result<LogRewrite, String> {
    prepare_log_change(path, |_, event| rewrite_metadata(event, previous, target))
}

fn prepare_log_change(path: &Path, mut change: impl FnMut(usize, &mut Value) -> usize) -> Result<LogRewrite, String> {
    let original = fs::read_to_string(path).map_err(|e| format!("无法读取会话日志：{e}"))?;
    let mut contents = String::with_capacity(original.len());
    let mut offsets = HashMap::from([(0, 0)]);
    let mut old_offset = 0; let mut replacements = 0;
    for (index, line) in original.split_inclusive('\n').enumerate() {
        let ending = if line.ends_with("\r\n") { "\r\n" } else if line.ends_with('\n') { "\n" } else { "" };
        let mut event: Value = serde_json::from_str(line).map_err(|e| format!("会话日志第 {} 行不是有效 JSONL：{e}", index + 1))?;
        let replaced = change(index, &mut event);
        if replaced == 0 { contents.push_str(line); } else {
            contents.push_str(&serde_json::to_string(&event).map_err(|e| e.to_string())?);
            contents.push_str(ending);
        }
        replacements += replaced; old_offset += line.len();
        offsets.insert(old_offset as i64, contents.len() as i64);
    }
    Ok(LogRewrite { path: path.to_path_buf(), original, existed: true, contents, replacements, offsets })
}

pub(super) fn prepare_log_restore(source: &Path, destination: &Path) -> Result<LogRewrite, String> {
    let mut rewrite = prepare_log_rewrite(source, &HashSet::new(), "")?;
    rewrite.path = destination.to_path_buf();
    rewrite.existed = destination.exists();
    rewrite.original = if rewrite.existed { fs::read_to_string(destination).map_err(|e| e.to_string())? } else { String::new() };
    rewrite.replacements = 1;
    Ok(rewrite)
}

pub(super) fn prepare_log_rollback(path: &Path, baseline: Option<&Path>, previous: &str, target: &str) -> Result<LogRewrite, String> {
    let original_events = if let Some(baseline) = baseline {
        fs::read_to_string(baseline).map_err(|e| e.to_string())?.lines().map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?
    } else { Vec::new() };
    prepare_log_change(path, |index, event| {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
        if let Some(before) = original_events.get(index).filter(|before| before.get("type") == event.get("type") && before.get("timestamp") == event.get("timestamp")) {
            let pointers: Vec<String> = if matches!(kind, "session_meta" | "turn_context") {
                METADATA_PATH_KEYS.iter().map(|key| format!("/payload/{key}")).collect()
            } else if kind == "world_state" { vec!["/payload/state/environments".into()] } else { Vec::new() };
            let mut changed = 0;
            for pointer in pointers {
                if let (Some(current), Some(original)) = (event.pointer_mut(&pointer), before.pointer(&pointer)) {
                    if current != original { *current = original.clone(); changed += 1; }
                }
            }
            if changed > 0 { return changed; }
        }
        rewrite_metadata(event, &HashSet::from([previous.to_string()]), target)
    })
}

pub(super) fn remap_history_offsets(conn: &Connection, schema: &str, thread_id: &str, rewrite: &LogRewrite) -> Result<(), String> {
    conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS rollout_offset_map (old_offset INTEGER PRIMARY KEY, new_offset INTEGER NOT NULL); DELETE FROM rollout_offset_map")
        .map_err(|e| e.to_string())?;
    {
        let mut insert = conn.prepare("INSERT INTO rollout_offset_map VALUES (?1, ?2)").map_err(|e| e.to_string())?;
        for (old, new) in &rewrite.offsets { insert.execute(params![old, new]).map_err(|e| e.to_string())?; }
    }
    for table in history_tables(conn, schema)? {
        for column in database_table_columns(conn, schema, &table)?.into_iter().filter(|column| column.contains("rollout") && column.ends_with("byte_offset")) {
            let table = format!("{}.{}", quote_sql_identifier(schema), quote_sql_identifier(&table));
            let column = quote_sql_identifier(&column);
            let invalid: bool = conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE thread_id = ?1 AND {column} IS NOT NULL AND {column} NOT IN (SELECT old_offset FROM rollout_offset_map))"), params![thread_id], |row| row.get(0)).map_err(|e| e.to_string())?;
            if invalid { return Err(format!("会话 {thread_id} 的分页历史与日志位置不一致，已取消操作；请退出 Codex 后重新导出或修复。")); }
            conn.execute(&format!("UPDATE {table} SET {column} = (SELECT new_offset FROM rollout_offset_map WHERE old_offset = {table}.{column}) WHERE thread_id = ?1 AND {column} IS NOT NULL"), params![thread_id]).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub(super) fn update_thread_path(conn: &Connection, id: &str, previous: &HashSet<String>, target: &str, original_policy: Option<&str>) -> Result<(), String> {
    let policy: String = conn.query_row("SELECT sandbox_policy FROM threads WHERE id = ?1", params![id], |row| row.get(0)).map_err(|e| e.to_string())?;
    let policy = if let Some(original) = original_policy { original.to_string() } else if let Ok(mut value) = serde_json::from_str::<Value>(&policy) {
        if remap_paths(&mut value, previous, target) > 0 { value.to_string() } else { policy }
    } else { policy };
    let updated = conn.execute("UPDATE threads SET cwd = ?1, sandbox_policy = ?2 WHERE id = ?3", params![target, policy, id]).map_err(|e| e.to_string())?;
    if updated != 1 { return Err(format!("数据库中已找不到会话 {id}，已取消操作。")); }
    Ok(())
}

pub(super) struct DesktopStateUpdate {
    pub path: PathBuf,
    pub original: String,
    pub existed: bool,
    pub state: Value,
}

pub(super) fn desktop_state_update(home: &Path) -> Result<Option<DesktopStateUpdate>, String> {
    let path = home.join(".codex-global-state.json");
    if !path.is_file() { return Ok(None); }
    let original = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let state = serde_json::from_str(&original).map_err(|e| format!("Codex 侧栏状态格式无效：{e}"))?;
    validate_desktop_state(&state)?;
    Ok(Some(DesktopStateUpdate { path, original, existed: true, state }))
}

pub(super) fn validate_desktop_state(state: &Value) -> Result<(), String> {
    if !state.is_object() { return Err("Codex 侧栏状态不是有效的对象。".into()); }
    for key in THREAD_PATH_KEYS.into_iter().chain(["thread-project-assignments", "local-projects", "project-appearances", "app-server-project-id-by-legacy-project-id-by-host"]) {
        if state.get(key).is_some_and(|value| !value.is_object()) { return Err(format!("Codex 侧栏状态字段 {key} 格式无效。")); }
    }
    if state.get("project-order").is_some_and(|value| !value.is_array()) { return Err("Codex 项目顺序状态格式无效。".into()); }
    if let Some(hosts) = state.get("app-server-project-id-by-legacy-project-id-by-host").and_then(Value::as_object) {
        if hosts.values().any(|value| !value.is_object()) { return Err("Codex 主机项目映射格式无效。".into()); }
    }
    Ok(())
}

pub(super) fn thread_path_settings(state: &Value, id: &str) -> HashMap<String, Value> {
    THREAD_PATH_KEYS.iter().filter_map(|key| state.get(key)?.get(id).cloned().map(|value| ((*key).to_string(), value))).collect()
}

pub(super) fn restore_thread_settings(state: &mut Value, id: &str, settings: &HashMap<String, Value>) {
    for key in THREAD_PATH_KEYS {
        if let Some(value) = settings.get(key) {
            if state.get(key).is_none() { state[key] = serde_json::json!({}); }
            state[key][id] = value.clone();
        }
    }
}

pub(super) fn desktop_project_for_thread(state: &Value, id: &str) -> Option<String> {
    let assignment = state.get("thread-project-assignments")?.get(id)?;
    if assignment.get("projectKind")?.as_str()? != "local" { return None; }
    let legacy = assignment.get("projectId")?.as_str()?;
    Some(state.get("app-server-project-id-by-legacy-project-id-by-host").and_then(Value::as_object)
        .and_then(|hosts| hosts.values().find_map(|projects| projects.get(legacy)))
        .and_then(Value::as_str).unwrap_or(legacy).to_string())
}

pub(super) fn restore_desktop_projects(state: &mut Value, backup: &Value, projects: &HashSet<String>) {
    for project in projects {
        for legacy in desktop_legacy_project_ids(backup, project) {
            for key in ["local-projects", "project-appearances"] {
                if let Some(value) = backup.get(key).and_then(|values| values.get(&legacy)) {
                    if state.get(key).is_none() { state[key] = serde_json::json!({}); }
                    if state[key].get(&legacy).is_none() { state[key][&legacy] = value.clone(); }
                }
            }
            if let Some(hosts) = backup.get("app-server-project-id-by-legacy-project-id-by-host").and_then(Value::as_object) {
                if state.get("app-server-project-id-by-legacy-project-id-by-host").is_none() { state["app-server-project-id-by-legacy-project-id-by-host"] = serde_json::json!({}); }
                for (host, mappings) in hosts {
                    if let Some(value) = mappings.get(&legacy) {
                        let target = &mut state["app-server-project-id-by-legacy-project-id-by-host"];
                        if target.get(host).is_none() { target[host] = serde_json::json!({}); }
                        if target[host].get(&legacy).is_none() { target[host][&legacy] = value.clone(); }
                    }
                }
            }
            if state.get("project-order").is_none() { state["project-order"] = serde_json::json!([]); }
            if let Some(order) = state["project-order"].as_array_mut() {
                if !order.iter().any(|id| id.as_str() == Some(&legacy)) { order.push(Value::String(legacy)); }
            }
        }
    }
}

// If a file write or commit fails, restore already-written files before returning.
pub(super) fn commit_with_logs(transaction: rusqlite::Transaction<'_>, rewrites: &[LogRewrite], desktop: Option<&DesktopStateUpdate>) -> Result<(), String> {
    let mut written = Vec::new();
    let mut desktop_written = false;
    let apply = (|| -> Result<(), String> {
        for rewrite in rewrites {
            if rewrite.replacements > 0 {
                atomic_write(&rewrite.path, &rewrite.contents)?;
                written.push(rewrite);
            }
        }
        if let Some(desktop) = desktop { atomic_write(&desktop.path, &desktop.state.to_string())?; desktop_written = true; }
        Ok(())
    })();
    let result = match apply { Ok(()) => transaction.commit().map_err(|e| e.to_string()), Err(error) => { drop(transaction); Err(error) } };
    if let Err(mut error) = result {
        for rewrite in written {
            let restore = if rewrite.existed { atomic_write(&rewrite.path, &rewrite.original) } else { fs::remove_file(&rewrite.path).map_err(|e| e.to_string()) };
            if let Err(restore_error) = restore { error.push_str(&format!("；恢复原日志失败：{restore_error}")); }
        }
        if let Some(desktop) = desktop.filter(|_| desktop_written) {
            let restore = if desktop.existed { atomic_write(&desktop.path, &desktop.original) } else { fs::remove_file(&desktop.path).map_err(|e| e.to_string()) };
            if let Err(restore_error) = restore { error.push_str(&format!("；恢复侧栏状态失败：{restore_error}")); }
        }
        return Err(error);
    }
    Ok(())
}
