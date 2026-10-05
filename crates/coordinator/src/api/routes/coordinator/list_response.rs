use crate::{
    api::routes::ApiError,
    domain::{Error, ListPage},
};
use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub ids: Option<String>,
    pub cursor: Option<Uuid>,
    pub limit: Option<usize>,
    pub status: Option<String>,
    pub since: Option<String>,
    pub updated_after: Option<String>,
    #[serde(default)]
    pub history: bool,
}

impl ListQuery {
    pub fn page(&self) -> Result<ListPage, ApiError> {
        let invalid = |text: &str| ApiError::from(Error::BadRequest(text.into()));
        let limit = self.limit.unwrap_or(50);
        if !(1..=100).contains(&limit) {
            return Err(invalid("limit must be between 1 and 100"));
        }
        if self.status.as_deref().is_some_and(|status| {
            ![
                "all",
                "active",
                "finished",
                "failed",
                "cancelled",
                "open",
                "live",
                "awaiting",
            ]
            .contains(&status)
        }) {
            return Err(invalid("unknown list status"));
        }
        let ids: Vec<Uuid> = self
            .ids
            .as_deref()
            .map(|ids| ids.split(',').map(Uuid::parse_str).collect())
            .transpose()
            .map_err(|_| invalid("ids must be comma-separated UUIDs"))?
            .unwrap_or_default();
        if ids.len() > 100 {
            return Err(invalid("at most 100 ids are allowed"));
        }
        let since = self
            .updated_after
            .as_ref()
            .or(self.since.as_ref())
            .map(|at| OffsetDateTime::parse(at, &Rfc3339))
            .transpose()
            .map_err(|_| invalid("since and updated_after must be RFC3339 timestamps"))?;
        Ok(ListPage {
            ids,
            before: self.cursor,
            since,
            status: self.status.clone(),
            history: self.history || self.status.as_deref() == Some("all"),
            limit,
        })
    }
}

/// The body keeps its array shape. A continuation header gives the next page's cursor.
pub fn response<T: Serialize>(
    headers: &HeaderMap,
    rows: &T,
    next: Option<Uuid>,
    private: bool,
) -> Result<Response, ApiError> {
    let bytes = serde_json::to_vec(rows)
        .map_err(|_| ApiError::Status(StatusCode::INTERNAL_SERVER_ERROR))?;
    let etag = format!("W/\"{:x}\"", Sha256::digest(&bytes));
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|value| value.trim() == etag || value.trim() == "*")
        });
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, "application/json")], bytes).into_response()
    };
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("hex digest"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if private {
            "private, no-cache"
        } else {
            "public, no-cache"
        }),
    );
    if let Some(cursor) = next {
        response.headers_mut().insert(
            "x-next-cursor",
            HeaderValue::from_str(&cursor.to_string()).expect("UUID"),
        );
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_page_inputs_are_refused_and_default_is_bounded() {
        assert_eq!(ListQuery::default().page().unwrap().limit, 50);
        for limit in [0, 101, usize::MAX] {
            assert!(ListQuery {
                limit: Some(limit),
                ..Default::default()
            }
            .page()
            .is_err());
        }
        assert!(ListQuery {
            ids: Some("not-a-uuid".into()),
            ..Default::default()
        }
        .page()
        .is_err());
        assert!(ListQuery {
            updated_after: Some("yesterday".into()),
            ..Default::default()
        }
        .page()
        .is_err());
        assert!(ListQuery {
            status: Some("unknown".into()),
            ..Default::default()
        }
        .page()
        .is_err());
    }

    #[test]
    fn unchanged_private_page_returns_304_and_keeps_its_cursor() {
        let cursor = Uuid::now_v7();
        let first = response(&HeaderMap::new(), &vec![1, 2], Some(cursor), true).unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let mut request = HeaderMap::new();
        request.insert(header::IF_NONE_MATCH, first.headers()[header::ETAG].clone());
        let same = response(&request, &vec![1, 2], Some(cursor), true).unwrap();
        assert_eq!(same.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(same.headers()[header::CACHE_CONTROL], "private, no-cache");
        assert_eq!(same.headers()["x-next-cursor"], cursor.to_string());
        assert_eq!(
            response(&request, &vec![1, 3], None, true)
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
}
