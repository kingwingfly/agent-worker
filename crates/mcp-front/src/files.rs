//! The file server: hands out the files the MCP server has saved.
//!
//! `GET /file?path=<path>` answers with the file at `path` as the MCP server reported
//! it, which is a path on this machine. Only regular files inside the one directory the
//! server was given exist as far as it is concerned; anything else is a 404.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    Router,
    body::Body,
    extract::{Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use tower::ServiceExt;
use tower_http::services::ServeFile;

/// The path every request for a file is made to.
pub const ROUTE: &str = "/file";

#[derive(Deserialize)]
struct Wanted {
    path: PathBuf,
}

/// A router serving the files under `dir`, which has to exist.
pub fn router(dir: &Path) -> io::Result<Router> {
    // Resolved once, so that a request can be checked against where the directory
    // really is, however the configuration or the MCP server spell its path.
    let dir = Arc::new(dir.canonicalize()?);
    Ok(Router::new().route(ROUTE, get(serve)).with_state(dir))
}

async fn serve(
    State(dir): State<Arc<PathBuf>>,
    Query(wanted): Query<Wanted>,
    request: Request,
) -> Response {
    let Some(file) = locate(&dir, &wanted.path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Ranges, HEAD, the content type and conditional requests are tower-http's.
    match ServeFile::new(file).oneshot(request).await {
        Ok(response) => response.map(Body::new),
        Err(never) => match never {},
    }
}

/// The file a request for `wanted` gets: a regular file that, once every symlink and
/// `..` in the path is followed, is inside `dir`. A relative path is taken from `dir`.
async fn locate(dir: &Path, wanted: &Path) -> Option<PathBuf> {
    let file = tokio::fs::canonicalize(dir.join(wanted)).await.ok()?;
    if !file.starts_with(dir) {
        return None;
    }
    let is_file = tokio::fs::metadata(&file).await.ok()?.is_file();
    is_file.then_some(file)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use axum::http::{Method, header};

    use super::*;

    struct Served {
        dir: tempfile::TempDir,
        router: Router,
    }

    /// A served directory `files` with one file in it, next to a file that is not served.
    fn served() -> Served {
        let dir = tempfile::tempdir().unwrap();
        let files = dir.path().join("files");
        fs::create_dir_all(files.join("sub")).unwrap();
        fs::write(files.join("report.txt"), "0123456789").unwrap();
        fs::write(files.join("sub/行程 (final).pdf"), "%PDF").unwrap();
        fs::write(dir.path().join("secret"), "no").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), files.join("link")).unwrap();
        let router = router(&files).unwrap();
        Served { dir, router }
    }

    impl Served {
        fn path(&self, relative: &str) -> String {
            self.dir.path().join(relative).display().to_string()
        }

        async fn get(&self, path: &str) -> (StatusCode, String) {
            self.request(Method::GET, path, None).await
        }

        async fn request(
            &self,
            method: Method,
            path: &str,
            range: Option<&str>,
        ) -> (StatusCode, String) {
            let query: String = path.bytes().map(|byte| format!("%{byte:02X}")).collect();
            let mut request = Request::builder()
                .method(method)
                .uri(format!("{ROUTE}?path={query}"));
            if let Some(range) = range {
                request = request.header(header::RANGE, range);
            }
            let response = self
                .router
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&body).into_owned())
        }
    }

    #[tokio::test]
    async fn a_file_is_served_by_the_path_the_mcp_server_reported() {
        let served = served();
        let (status, body) = served.get(&served.path("files/report.txt")).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "0123456789"));

        // Names are whatever the MCP server was given: spaces, brackets, any script.
        let (status, body) = served.get(&served.path("files/sub/行程 (final).pdf")).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "%PDF"));

        // A path that is not absolute is taken from the served directory.
        let (status, body) = served.get("report.txt").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "0123456789"));
    }

    #[tokio::test]
    async fn nothing_outside_the_directory_is_served() {
        let served = served();
        for path in [
            served.path("secret"),
            served.path("files/../secret"),
            served.path("files/link"),
            "../secret".to_owned(),
            "/etc/hostname".to_owned(),
            served.path("files/missing.txt"),
            served.path("files/sub"),
            served.path("files"),
        ] {
            let (status, body) = served.get(&path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert!(!body.contains("no"), "{path}");
        }
    }

    #[tokio::test]
    async fn parts_of_a_file_can_be_asked_for() {
        let served = served();
        let path = served.path("files/report.txt");
        let (status, body) = served.request(Method::GET, &path, Some("bytes=2-5")).await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::PARTIAL_CONTENT, "2345")
        );

        let (status, body) = served.request(Method::HEAD, &path, None).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, ""));
    }

    #[tokio::test]
    async fn files_can_only_be_read() {
        let served = served();
        let path = served.path("files/report.txt");
        for method in [Method::POST, Method::PUT, Method::DELETE] {
            let (status, _) = served.request(method, &path, None).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        }
        // A request that does not say which file is the caller's mistake.
        let response = served
            .router
            .clone()
            .oneshot(Request::builder().uri(ROUTE).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
