use std::sync::Arc;
use axum::{extract::{Query, State}, Json};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

pub async fn state(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<R<serde_json::Value>>, AppError> {
    Err(AppError::validation("not implemented yet"))
}

pub async fn admin_refill(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<R<serde_json::Value>>, AppError> {
    Err(AppError::validation("not implemented yet"))
}

pub async fn admin_remove(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<R<serde_json::Value>>, AppError> {
    Err(AppError::validation("not implemented yet"))
}