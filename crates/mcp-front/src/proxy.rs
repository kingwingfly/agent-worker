//! The one door into the container: a pingora proxy that sends each request to the
//! server it is for.

use std::net::SocketAddr;

use async_trait::async_trait;
use pingora::{
    Result,
    proxy::{ProxyHttp, Session},
    upstreams::peer::HttpPeer,
};

use crate::files;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    Files,
    Mcp,
}

/// Which server a request path is for: the file server's route and everything below it
/// go to the file server, and all else is the MCP server's business.
pub fn upstream_for(path: &str) -> Upstream {
    match path.strip_prefix(files::ROUTE) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => Upstream::Files,
        _ => Upstream::Mcp,
    }
}

/// Requests are passed on as they are, `Host` included: an rmcp server only answers to
/// the loopback names, and that is what callers of this container use.
pub struct Front {
    pub mcp: SocketAddr,
    pub files: SocketAddr,
}

#[async_trait]
impl ProxyHttp for Front {
    type CTX = ();

    fn new_ctx(&self) {}

    async fn upstream_peer(&self, session: &mut Session, _ctx: &mut ()) -> Result<Box<HttpPeer>> {
        let addr = match upstream_for(session.req_header().uri.path()) {
            Upstream::Files => self.files,
            Upstream::Mcp => self.mcp,
        };
        Ok(Box::new(HttpPeer::new(addr, false, String::new())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_file_route_goes_to_the_file_server() {
        for path in ["/file", "/file/", "/file/a/b"] {
            assert_eq!(upstream_for(path), Upstream::Files, "{path}");
        }
        for path in [
            "/",
            "/mcp",
            "/mcp/file",
            "/files",
            "/file.txt",
            "/filesystem/x",
            "",
        ] {
            assert_eq!(upstream_for(path), Upstream::Mcp, "{path}");
        }
    }
}
