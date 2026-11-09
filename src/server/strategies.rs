//! The strategy archive routes (r7.s1.w2, PDR-0051 decision 3): archive,
//! unarchive and the Library read that lists archived strategies too.
//!
//! All three mount through the `mount_*` helpers with [`Scope::App`](super::auth::Scope), so an
//! `agent` token is a 403 `scope_refused` on each (least privilege, ADR-0026).
//! Archiving deletes nothing: it flips the strategy's `archived` flag, and the
//! call is idempotent. The request log names who archived (the token's label);
//! no actor is read from the body.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::paper::api_error;
use super::routes::MountExt;
use super::{ServerState, ops};
use crate::domain::StrategyRepository as _;
use crate::domain::strategy::StrategyId;
use crate::tauri::StrategyArchiveResult;
use crate::tauri::commands::library_overview_core;

/// Mount the archive, unarchive and with-archived Library routes. All of them
/// are `Scope::App` (the helpers enforce it).
pub(crate) fn mount(router: axum::Router, state: &Arc<ServerState>) -> axum::Router {
    router
        .mount_post_path(
            state,
            "/api/v1/strategies/{id}/archive",
            |state, id, _req| Box::pin(archive_route(state, id)),
        )
        .mount_post_path(
            state,
            "/api/v1/strategies/{id}/unarchive",
            |state, id, _req| Box::pin(unarchive_route(state, id)),
        )
        .mount_get(state, "/api/v1/library-overview/with-archived", |state| {
            Box::pin(with_archived_route(state))
        })
}

/// `POST /api/v1/strategies/{id}/archive` — hide one strategy from the default
/// Library read. Nothing is deleted.
async fn archive_route(state: Arc<ServerState>, id: String) -> Response {
    set_archived(&state, &id, true).await
}

/// `POST /api/v1/strategies/{id}/unarchive` — list one strategy in the default
/// Library read again.
async fn unarchive_route(state: Arc<ServerState>, id: String) -> Response {
    set_archived(&state, &id, false).await
}

/// `GET /api/v1/library-overview/with-archived` — the Library read with
/// archived strategies included (the app's Archived filter).
async fn with_archived_route(state: Arc<ServerState>) -> Response {
    match library_overview_core(&state.desktop, true).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(err) => ops::bus_422(&err),
    }
}

/// Set one strategy's archived flag. An unknown id is a 404 `not_found`: the
/// store's own error for it is untyped, so the strategy is read first.
async fn set_archived(state: &ServerState, id: &str, archived: bool) -> Response {
    let repo = state.desktop.strategy_repo();
    let strategy_id = StrategyId::new(id.to_owned());
    match repo.get_strategy(&strategy_id).await {
        Ok(None) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no strategy with id {id}"),
            );
        }
        Ok(Some(_)) => {}
        Err(error) => return archive_failed(&error),
    }
    match repo.archive_strategy(&strategy_id, archived).await {
        Ok(strategy) => (
            StatusCode::OK,
            axum::Json(StrategyArchiveResult {
                id: strategy.id.as_str().to_owned(),
                archived: strategy.archived,
            }),
        )
            .into_response(),
        Err(error) => archive_failed(&error),
    }
}

/// The 500 a store failure answers with.
fn archive_failed(error: &crate::domain::DataError) -> Response {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "data",
        format!("strategy archive failed: {error}"),
    )
}
