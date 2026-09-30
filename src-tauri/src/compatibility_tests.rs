use super::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("csm-compat-{}-{}-{serial}", std::process::id(), Local::now().timestamp_nanos_opt().unwrap()));
        fs::create_dir(&path).unwrap(); Self(fs::canonicalize(path).unwrap())
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

fn create_history(conn: &Connection, schema: &str) {
    conn.execute_batch(&format!(
        "CREATE TABLE {schema}.thread_turns (thread_id TEXT, turn_id TEXT, rollout_byte_offset INTEGER, rollout_end_byte_offset INTEGER, PRIMARY KEY(thread_id, turn_id));
         CREATE TABLE {schema}.thread_items (thread_id TEXT, item_id TEXT, item_json TEXT, PRIMARY KEY(thread_id, item_id));
         CREATE TABLE {schema}.thread_realtime_items (thread_id TEXT, item_id TEXT, PRIMARY KEY(thread_id, item_id));
         CREATE TABLE {schema}.thread_history_projection_state (thread_id TEXT PRIMARY KEY, next_rollout_byte_offset INTEGER NOT NULL, next_rollout_ordinal INTEGER NOT NULL);
         CREATE TABLE {schema}._sqlx_migrations (version INTEGER PRIMARY KEY);
         INSERT INTO {schema}._sqlx_migrations VALUES (7);"
    )).unwrap();
}

#[test]
fn modern_path_repair_preserves_unrelated_roots_chat_bytes_and_newlines() {
    let fixture = Fixture::new(); let path = fixture.0.join("session.jsonl");
    let context = serde_json::json!({"type":"turn_context","ordinal":1,"payload": {
        "cwd":"/旧项目", "workspace_roots":["/旧项目", "/其他项目", "/旧项目2"],
        "permission_profile":{"file_system":{"entries":[{"path":{"type":"path","path":"/旧项目/.git"},"access":"read"},{"path":{"type":"special","value":{"kind":"root"}},"access":"read"}]}},
        "sandbox_policy":{"writable_roots":["/旧项目", "/tmp"]}
    }});
    let chat = "{ \"type\":\"response_item\", \"payload\": {\"cwd\":\"/旧项目\",\"text\":\"[链接](/旧项目/a.md)\"} }\r\n";
    let world = serde_json::json!({"type":"world_state","ordinal":3,"payload":{"state":{"environments":{"environments":{"local":{"cwd":"/旧项目"}},"filesystem":"<filesystem><root>/旧项目</root><path>/旧项目/.agents</path><special>:root</special></filesystem>"},"permissions":{"instructions":"/旧项目"}}}});
    fs::write(&path, format!("{context}\r\n{chat}{world}")).unwrap();
    let rewrite = prepare_log_rewrite(&path, &HashSet::from(["/旧项目".into()]), "/新目录更长").unwrap();
    let lines = rewrite.contents.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).collect::<Vec<_>>();
    assert_eq!(lines[0]["payload"]["workspace_roots"], serde_json::json!(["/新目录更长", "/其他项目", "/旧项目2"]));
    assert_eq!(lines[0]["payload"]["permission_profile"]["file_system"]["entries"][0]["path"]["path"], "/新目录更长/.git");
    assert_eq!(lines[0]["payload"]["permission_profile"]["file_system"]["entries"][0]["access"], "read");
    assert_eq!(lines[0]["payload"]["sandbox_policy"]["writable_roots"][1], "/tmp");
    assert!(rewrite.contents.contains(chat)); assert!(!rewrite.contents.ends_with('\n'));
    assert_eq!(lines[2]["payload"]["state"]["environments"]["environments"]["local"]["cwd"], "/新目录更长");
    assert_eq!(lines[2]["payload"]["state"]["permissions"]["instructions"], "/旧项目");
    let old_end = fs::read(&path).unwrap().len() as i64;
    assert_eq!(rewrite.offsets[&old_end], rewrite.contents.len() as i64);
    assert_eq!(mapped_path(r"C:\Old\child", &HashSet::from(["c:/old".into()]), "/new"), Some("/new/child".into()));
    assert_eq!(mapped_path(r"C:\Older", &HashSet::from(["c:/old".into()]), "/new"), None);
}

#[test]
fn snapshots_and_copies_only_selected_history_and_preserves_migrations() {
    let fixture = Fixture::new(); let home = fixture.0.join("home"); let package = fixture.0.join("package");
    fs::create_dir(&home).unwrap(); fs::create_dir(&package).unwrap();
    let source = Connection::open(home.join(HISTORY_FILE)).unwrap(); create_history(&source, "main");
    source.execute_batch("INSERT INTO thread_turns VALUES ('kept','t',0,100), ('other','t',0,200); INSERT INTO thread_items VALUES ('kept','i','kept payload'), ('other','i','private payload'); INSERT INTO thread_history_projection_state VALUES ('kept',100,2), ('other',200,3)").unwrap();
    snapshot_history(&home, &package, &["kept".into()]).unwrap();
    let snapshot = Connection::open(package.join(HISTORY_FILE)).unwrap();
    assert_eq!(snapshot.query_row("SELECT COUNT(*) FROM thread_items", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(snapshot.query_row("SELECT version FROM _sqlx_migrations", [], |row| row.get::<_, i64>(0)).unwrap(), 7);
    assert_eq!(source.query_row("SELECT COUNT(*) FROM thread_items", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
    assert!(!fs::read(package.join(HISTORY_FILE)).unwrap().windows(b"private payload".len()).any(|bytes| bytes == b"private payload"));
    let mut target = Connection::open_in_memory().unwrap();
    target.execute_batch("ATTACH ':memory:' AS thread_history; ATTACH ':memory:' AS incoming_history; CREATE TEMP TABLE scope(id TEXT PRIMARY KEY); INSERT INTO scope VALUES ('kept')").unwrap();
    create_history(&target, "thread_history"); create_history(&target, "incoming_history");
    target.execute_batch("INSERT INTO incoming_history.thread_items VALUES ('kept','i','kept payload'),('other','i','private payload'); INSERT INTO thread_history.thread_items VALUES ('kept','old','stale'),('existing','i','local payload')").unwrap();
    let transaction = target.transaction().unwrap(); copy_history(&transaction, "scope").unwrap(); transaction.commit().unwrap();
    assert_eq!(target.query_row("SELECT COUNT(*) FROM thread_history.thread_items", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
    assert_eq!(target.query_row("SELECT item_json FROM thread_history.thread_items WHERE thread_id='existing'", [], |row| row.get::<_, String>(0)).unwrap(), "local payload");
}

#[test]
fn newer_history_fields_fail_without_discarding_existing_cache() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("ATTACH ':memory:' AS thread_history; ATTACH ':memory:' AS incoming_history; CREATE TEMP TABLE scope(id TEXT); INSERT INTO scope VALUES('kept')").unwrap();
    create_history(&conn, "thread_history"); create_history(&conn, "incoming_history");
    conn.execute_batch("INSERT INTO thread_history.thread_items VALUES ('kept','existing','local content'); ALTER TABLE incoming_history.thread_items ADD COLUMN newer_field TEXT; INSERT INTO incoming_history.thread_items VALUES ('kept','new','imported content','required metadata')").unwrap();
    {
        let transaction = conn.transaction().unwrap();
        assert!(copy_history(&transaction, "scope").unwrap_err().contains("缺少源版本字段"));
    }
    assert_eq!(conn.query_row("SELECT item_json FROM thread_history.thread_items WHERE thread_id='kept'", [], |row| row.get::<_, String>(0)).unwrap(), "local content");
}

#[test]
fn offsets_follow_utf8_log_changes_and_invalid_offsets_roll_back_metadata() {
    let fixture = Fixture::new(); let path = fixture.0.join("session.jsonl");
    fs::write(&path, "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/old\"}}\n{\"type\":\"event_msg\",\"payload\":{\"text\":\"保持中文\"}}\n").unwrap();
    let rewrite = prepare_log_rewrite(&path, &HashSet::from(["/old".into()]), "/新的更长路径").unwrap();
    let old_end = rewrite.original.len() as i64;
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE threads(id TEXT, cwd TEXT); INSERT INTO threads VALUES ('kept','/old'); ATTACH ':memory:' AS thread_history").unwrap(); create_history(&conn, "thread_history");
    conn.execute("INSERT INTO thread_history.thread_turns VALUES ('kept','t',0,?1)", params![old_end]).unwrap();
    conn.execute("INSERT INTO thread_history.thread_history_projection_state VALUES ('kept',?1,2)", params![old_end]).unwrap();
    remap_history_offsets(&conn, "thread_history", "kept", &rewrite).unwrap();
    assert_eq!(conn.query_row("SELECT next_rollout_byte_offset FROM thread_history.thread_history_projection_state", [], |row| row.get::<_, i64>(0)).unwrap(), rewrite.contents.len() as i64);
    conn.execute_batch("UPDATE thread_history.thread_history_projection_state SET next_rollout_byte_offset=1").unwrap();
    {
        let transaction = conn.transaction().unwrap(); transaction.execute_batch("UPDATE threads SET cwd='/changed'").unwrap();
        assert!(remap_history_offsets(&transaction, "thread_history", "kept", &rewrite).unwrap_err().contains("位置不一致"));
    }
    assert_eq!(conn.query_row("SELECT cwd FROM threads", [], |row| row.get::<_, String>(0)).unwrap(), "/old");
    assert_eq!(fs::read_to_string(&path).unwrap(), rewrite.original);
}

#[test]
fn failed_file_write_restores_written_logs_and_rolls_back_sql() {
    let fixture = Fixture::new(); let good = fixture.0.join("good.jsonl"); let blocked = fixture.0.join("blocked.jsonl");
    fs::write(&good, "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/old\"}}\n").unwrap(); fs::create_dir(&blocked).unwrap();
    let rewrite = prepare_log_rewrite(&good, &HashSet::from(["/old".into()]), "/new").unwrap(); let original = rewrite.original.clone();
    let failing = LogRewrite { path: blocked, original: String::new(), existed: false, contents: "data".into(), replacements: 1, offsets: HashMap::new() };
    let mut conn = Connection::open_in_memory().unwrap(); conn.execute_batch("CREATE TABLE threads(cwd TEXT); INSERT INTO threads VALUES('/old')").unwrap();
    let transaction = conn.transaction().unwrap(); transaction.execute_batch("UPDATE threads SET cwd='/new'").unwrap();
    assert!(commit_with_logs(transaction, &[rewrite, failing], None).is_err());
    assert_eq!(fs::read_to_string(&good).unwrap(), original);
    assert_eq!(conn.query_row("SELECT cwd FROM threads", [], |row| row.get::<_, String>(0)).unwrap(), "/old");
}

#[test]
fn rollback_restores_original_metadata_and_preserves_later_messages() {
    let fixture = Fixture::new(); let log = fixture.0.join("session.jsonl"); let backup = fixture.0.join("backup.jsonl");
    let metadata = "{\"timestamp\":\"t\",\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/old\",\"workspace_roots\":[\"/old\",\"/old/sub\",\"/separate\"]}}\n";
    fs::write(&log, metadata).unwrap(); fs::copy(&log, &backup).unwrap();
    let repaired = prepare_log_rewrite(&log, &HashSet::from(["/old".into()]), "/new-longer").unwrap();
    let message = "{ \"type\":\"response_item\",\"payload\":{\"text\":\"new message in /new-longer\"}}\n";
    fs::write(&log, format!("{}{message}", repaired.contents)).unwrap();
    let rollback = prepare_log_rollback(&log, Some(&backup), "/new-longer", "/old").unwrap();
    assert!(rollback.contents.ends_with(message));
    let restored: Value = serde_json::from_str(rollback.contents.lines().next().unwrap()).unwrap();
    let before: Value = serde_json::from_str(metadata).unwrap(); assert_eq!(restored, before);
}

#[test]
fn legacy_packages_remain_valid_but_incomplete_paginated_packages_fail() {
    let fixture = Fixture::new(); let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE threads(id TEXT, history_mode TEXT); INSERT INTO threads VALUES('legacy','legacy'),('new','paginated')").unwrap();
    let missing = fixture.0.join(HISTORY_FILE);
    validate_history(&conn, "main", &missing, &["legacy".into()]).unwrap();
    assert!(validate_history(&conn, "main", &missing, &["new".into()]).unwrap_err().contains("缺少 thread_history_1.sqlite"));
    let history = Connection::open(&missing).unwrap(); create_history(&history, "main");
    assert!(validate_history(&conn, "main", &missing, &["new".into()]).unwrap_err().contains("日志索引"));
    history.execute_batch("INSERT INTO thread_history_projection_state VALUES ('new',0,0)").unwrap();
    validate_history(&conn, "main", &missing, &["new".into()]).unwrap();
}

#[test]
#[cfg(target_os = "macos")]
fn prefers_current_bundled_cli_over_legacy_paths_and_path_fallback() {
    let candidates = codex_executable_candidates();
    let current = candidates.iter().position(|path| path == Path::new("/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex")).unwrap();
    let legacy = candidates.iter().position(|path| path == Path::new("/Applications/ChatGPT.app/Contents/Resources/codex")).unwrap();
    let fallback = candidates.iter().position(|path| path == Path::new("codex")).unwrap(); assert!(current < legacy && legacy < fallback);
}

#[test]
#[cfg(unix)]
fn managed_logs_handle_directory_aliases_and_reject_external_symlinks() {
    let fixture = Fixture::new(); let home = fixture.0.join("home");
    fs::create_dir_all(home.join("sessions")).unwrap(); let alias = fixture.0.join("alias");
    std::os::unix::fs::symlink(&home, &alias).unwrap();
    let real_log = home.join("sessions/session.jsonl"); fs::write(&real_log, "{}").unwrap();
    assert!(same_file_path(&real_log, &alias.join("sessions/session.jsonl")));
    assert!(is_managed_log(&alias, &real_log));
    assert!(is_managed_log(&home, &alias.join("sessions/missing/sub/log.jsonl")));
    let outside = fixture.0.join("outside.jsonl"); fs::write(&outside, "{}").unwrap();
    let linked = home.join("sessions/link.jsonl"); std::os::unix::fs::symlink(outside, &linked).unwrap();
    assert!(!is_managed_log(&home, &linked));
    assert!(!is_managed_log(&home, &home.join("sessions/../../outside.jsonl")));
}

fn read_page_with_bundled_codex(home: &Path, id: &str) -> Vec<Value> {
    let executable = codex_executable_candidates().into_iter().find(|path| path.to_string_lossy().contains("codex-cli/bin") && path.is_file()).expect("a current bundled Codex CLI is required");
    let error_path = home.join("csm-test-stderr.log");
    let mut child = Command::new(executable).args(["app-server", "--stdio"]).env("CODEX_HOME", home)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(fs::File::create(&error_path).unwrap()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap(); let mut reader = BufReader::new(child.stdout.take().unwrap());
    writeln!(stdin, "{}", serde_json::json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"csm-test","version":"1.3.0"},"capabilities":{"experimentalApi":true}}})).unwrap(); stdin.flush().unwrap();
    let initialize = wait_for_app_server_response(&mut reader, 1);
    if let Err(error) = initialize { let _ = child.kill(); let _ = child.wait(); panic!("{error}; {}", fs::read_to_string(error_path).unwrap()); }
    writeln!(stdin, "{}", serde_json::json!({"method":"initialized"})).unwrap();
    writeln!(stdin, "{}", serde_json::json!({"id":2,"method":"thread/turns/list","params":{"threadId":id,"limit":100,"itemsView":"full"}})).unwrap(); stdin.flush().unwrap();
    let response = wait_for_app_server_response(&mut reader, 2); let _ = child.kill(); let _ = child.wait();
    let response = response.unwrap(); assert!(response.get("error").is_none(), "app-server returned an error: {:?}", response.get("error"));
    response["result"]["data"].as_array().unwrap().clone()
}

#[test]
#[ignore = "reads local Codex data into isolated copies and runs the installed CLI only against those copies"]
fn current_codex_export_import_repair_rollback_delete_restore_round_trip() {
    let real_home = codex_home().unwrap();
    let source = Connection::open_with_flags(state_database(&real_home).unwrap(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let candidates = query_threads(&source).unwrap().into_iter().filter(|thread| thread.source == "vscode" && !thread.archived && Path::new(&thread.rollout_path).is_file()).collect::<Vec<_>>();
    let mut selected = vec![candidates.last().unwrap().clone()];
    if let Some(fork) = candidates.iter().find(|thread| {
        let file = fs::File::open(&thread.rollout_path).unwrap();
        BufReader::new(file).lines().take(80).flatten().filter_map(|line| serde_json::from_str::<Value>(&line).ok())
            .any(|event| event.get("type").and_then(Value::as_str) == Some("session_meta") && event.pointer("/payload/forked_from_id").is_some())
    }) { if fork.id != selected[0].id { selected.push(fork.clone()); } }
    for thread in selected {
    let fixture = Fixture::new();
    let home = fixture.0.join("source"); fs::create_dir(&home).unwrap();
    backup_database(&state_database(&real_home).unwrap(), &home).unwrap();
    snapshot_history(&real_home, &home, std::slice::from_ref(&thread.id)).unwrap();
    let log = home.join("sessions").join(Path::new(&thread.rollout_path).file_name().unwrap()); fs::create_dir(log.parent().unwrap()).unwrap(); fs::copy(&thread.rollout_path, &log).unwrap();
    let db = Connection::open(home.join("state_5.sqlite")).unwrap();
    // Keep the installed schema/migration ledger, but remove all unrelated data.
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name!='_sqlx_migrations'").unwrap()
        .query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    for table in tables {
        let columns = database_table_columns(&db, "main", &table).unwrap();
        if table == "threads" { db.execute("DELETE FROM threads WHERE id!=?1", params![thread.id]).unwrap(); }
        else if columns.iter().any(|column| column == "thread_id") { db.execute(&format!("DELETE FROM {} WHERE thread_id!=?1", quote_sql_identifier(&table)), params![thread.id]).unwrap(); }
        else { db.execute(&format!("DELETE FROM {}", quote_sql_identifier(&table)), []).unwrap(); }
    }
    db.execute_batch("UPDATE threads SET project_id=NULL,thread_section_id=NULL").unwrap();
    db.execute("UPDATE threads SET rollout_path=?1 WHERE id=?2", params![display_path(&log), thread.id]).unwrap();
    db.execute("INSERT INTO projects(id,name,position,created_at_ms,updated_at_ms) VALUES('fixture-project','Fixture',0,1,1)", []).unwrap();
    db.execute("INSERT INTO project_roots(project_id,position,path) VALUES('fixture-project',0,?1)", params![thread.cwd]).unwrap();
    db.execute("UPDATE threads SET project_id='fixture-project'", []).unwrap(); drop(db);
    fs::write(home.join(".codex-global-state.json"), serde_json::json!({"thread-writable-roots":{&thread.id:[thread.cwd]},"thread-workspace-root-hints":{&thread.id:[thread.cwd]}}).to_string()).unwrap();
    let baseline = read_page_with_bundled_codex(&home, &thread.id); assert!(!baseline.is_empty()); eprintln!("baseline history readable");
    let exported = export_sessions_at(&home, ExportSessionsRequest { thread_ids: vec![thread.id.clone()], destination_directory: display_path(&fixture.0) }).unwrap();
    let manifest_path = PathBuf::from(&exported.backup_folder).join("manifest.json"); let manifest = read_export_manifest(&manifest_path).unwrap(); assert_eq!(manifest.version, 2); assert!(manifest.history_database.is_some());
    let destination = fixture.0.join("destination"); fs::create_dir(&destination).unwrap();
    backup_database(&home.join("state_5.sqlite"), &destination).unwrap();
    let empty = Connection::open(destination.join("state_5.sqlite")).unwrap(); empty.execute_batch("PRAGMA foreign_keys=OFF; DELETE FROM thread_spawn_edges; DELETE FROM thread_dynamic_tools; DELETE FROM thread_attachments; DELETE FROM threads").unwrap(); drop(empty);
    let workspace = fixture.0.join("新电脑项目"); let repaired_workspace = fixture.0.join("修复后更长的项目目录"); fs::create_dir(&workspace).unwrap(); fs::create_dir(&repaired_workspace).unwrap();
    import_sessions_at(&destination, ImportSessionsRequest { manifest_path: display_path(&manifest_path), confirmation: "IMPORT".into(), project_mappings: vec![ImportProjectMapping { project_id: "fixture-project".into(), target_path: display_path(&workspace) }] }).unwrap();
    eprintln!("export/import completed");
    assert!(read_page_with_bundled_codex(&destination, &thread.id) == baseline, "import changed persisted turn content");
    let repair = repair_session_at(&destination, RepairRequest { thread_id: thread.id.clone(), target_path: display_path(&repaired_workspace), confirmation: "REPAIR".into(), backup_base: None, include_child_agents: false }).unwrap();
    eprintln!("repair completed");
    assert!(read_page_with_bundled_codex(&destination, &thread.id) == baseline, "repair changed persisted turn content");
    rollback_repair_at(&destination, RollbackRequest { manifest_path: display_path(&PathBuf::from(repair.backup_folder).join("repair-history.json")), backup_base: None, confirmation: "ROLLBACK".into() }).unwrap();
    eprintln!("repair rollback completed");
    assert!(read_page_with_bundled_codex(&destination, &thread.id) == baseline, "repair rollback changed persisted turn content");
    let deleted = delete_session_at(&destination, DeleteSessionRequest { thread_id: thread.id.clone(), confirmation: "DELETE".into(), backup_base: None }).unwrap();
    eprintln!("delete completed");
    let current = Connection::open(destination.join("state_5.sqlite")).unwrap(); assert_eq!(current.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    current.execute_batch("DELETE FROM project_roots; DELETE FROM projects").unwrap(); drop(current);
    purge_desktop_project(&destination, "fixture-project").unwrap();
    rollback_delete_at(&destination, DeleteRollbackRequest { manifest_path: display_path(&PathBuf::from(deleted.backup_folder).join("delete-history.json")), backup_base: None, confirmation: "RESTORE_DELETION".into() }).unwrap();
    eprintln!("delete restore completed");
    assert!(read_page_with_bundled_codex(&destination, &thread.id) == baseline, "deletion restore changed persisted turn content");
    let current = Connection::open(destination.join("state_5.sqlite")).unwrap();
    assert_eq!(current.query_row("SELECT cwd FROM threads WHERE id=?1", params![thread.id], |row| row.get::<_, String>(0)).unwrap(), display_path(&workspace));
    let sidebar = desktop_state_update(&destination).unwrap().unwrap(); assert!(desktop_project_for_thread(&sidebar.state, &thread.id).is_some());
    assert_eq!(current.query_row("SELECT COUNT(*) FROM projects WHERE id='fixture-project'", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    }
}
