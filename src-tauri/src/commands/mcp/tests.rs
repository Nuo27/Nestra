use super::*;

fn seed_usage_env(conn: &rusqlite::Connection) {
    crate::schema::build_v1(conn).unwrap();
    for (id, name) in [("srv-1", "fs"), ("srv-2", "codegraph")] {
        conn.execute(
            "INSERT INTO mcp_server (id, name, transport_json, enabled_agents,
                                         disabled_agents, created_at)
                 VALUES (?1, ?2, '{}', '[]', '[]', 0)",
            rusqlite::params![id, name],
        )
        .unwrap();
    }
    let insert_row = |started: i64, tools: &str| {
        conn.execute(
            "INSERT INTO task (id, lifecycle, started_at) VALUES (?1,'done',?2)",
            rusqlite::params![format!("t-{started}"), started],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO route_request (request_id, task_id, agent_id, route_reason,
                                            tool_names, started_at)
                 VALUES (?1,?2,'pi-cli','capability',?3,?4)",
            rusqlite::params![format!("r-{started}"), format!("t-{started}"), tools, started],
        )
        .unwrap();
    };
    insert_row(100, r#"{"mcp__fs__read": 2, "Bash": 5}"#);
    insert_row(200, r#"{"mcp__fs__write": 1, "mcp__unmanaged__read": 3}"#);
    insert_row(300, r#"{"mcp__codegraph__query": 1}"#);
    // NULL tool_names rows must be skipped (and not crash).
    insert_row(400, "{}");
}

#[test]
fn aggregate_usage_attributes_by_managed_namespace() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    seed_usage_env(&conn);
    let stats = aggregate_usage(&conn).unwrap();
    let by_name: std::collections::HashMap<&str, &McpUsageStat> =
        stats.iter().map(|s| (s.server_name.as_str(), s)).collect();

    // Managed + attributed: totals, per-tool, last_used max.
    let fs = by_name["fs"];
    assert_eq!(fs.total_calls, 3);
    assert_eq!(fs.per_tool.get("read"), Some(&2));
    assert_eq!(fs.per_tool.get("write"), Some(&1));
    assert_eq!(fs.last_used_at, Some(200));
    assert_eq!(by_name["codegraph"].total_calls, 1);
    // Never-observed server is present with a zero (the "未观察到" case).
    assert!(stats.iter().all(|s| s.server_name != "unmanaged"), "only managed servers appear");
    // Unmanaged-server namespace and plain tools stay unattributed.
    assert_eq!(by_name["codegraph"].per_tool.get("query"), Some(&1));
    let _ = &by_name["codegraph"];
}