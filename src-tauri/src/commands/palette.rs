use crate::error::{AppError, AppResult};
use crate::session::store;
use crate::{db, skills};
use serde::Serialize;
use super::run_blocking;
use tauri::State;

#[derive(Serialize)]
pub struct PaletteItem {
    pub kind: &'static str, // "provider" | "session" | "skill" — nav entries
                            // are injected frontend-side (i18n'd, route-accurate)
    pub label: String,
    pub detail: Option<String>,
    pub target: String,
}

// ---- Palette command ----

/// Minimal URL-component escape for SPA hash-route fragments (no path-or-host
/// semantics needed). Forwards to `url::form_urlencoded::byte_serialize`.
fn url_escape(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[tauri::command]
pub async fn palette_search(
    state: State<'_, crate::AppState>,
    query: String,
) -> AppResult<Vec<PaletteItem>> {
    let db = state.db_read.clone();
    run_blocking(move || {
        let mut items: Vec<PaletteItem> = Vec::new();
        // Pure read on db_read — the launch reconcile runs in the background
        // on its own connection, so palette never waits on it.
        let conn = match db.lock() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("palette: db lock failed: {e}");
                return Err(AppError::Internal(e.to_string()));
            }
        };
        if let Ok(endpoints) = db::list_endpoints(&conn) {
            for e in endpoints {
                items.push(PaletteItem {
                    kind: "provider",
                    label: e.display_name.clone(),
                    detail: Some(if e.has_api_key { e.status.clone() } else { "no key".into() }),
                    target: format!("/providers/{}", e.id),
                });
            }
        }
        // Sessions come from the reconciled store.
        if let Ok(sessions) = store::list_sessions(&conn, None, None, 20) {
            for s in sessions {
                items.push(PaletteItem {
                    kind: "session",
                    label: s.title.clone(),
                    detail: Some(s.provider.clone()),
                    target: format!(
                        "/sessions?id={}&provider={}",
                        url_escape(&s.id),
                        url_escape(&s.provider),
                    ),
                });
            }
        }
        if let Ok(ss) = skills::list(&conn) {
            for s in ss {
                items.push(PaletteItem {
                    kind: "skill",
                    label: s.name.clone(),
                    detail: Some(s.source.clone()),
                    target: format!("/skills#{}", url_escape(&s.path)),
                });
            }
        }
        if !query.is_empty() {
            let q = query.to_lowercase();
            items.retain(|i| {
                i.label.to_lowercase().contains(&q)
                    || i.target.to_lowercase().contains(&q)
                    || i.detail.as_deref().is_some_and(|d| d.to_lowercase().contains(&q))
            });
        }
        items.truncate(50);
        Ok(items)
    })
    .await
}
