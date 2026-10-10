# The shared MCP server. This builds the placeholder from crates/mcp-example; replace it
# with a build of your own server, or delete it and set `image` in wrangler.toml to an
# image in a registry. Either way the server must listen on 0.0.0.0:8080 and serve MCP
# over streamable HTTP at /mcp. The build context is the repository root.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p agent-mcp-example

FROM debian:bookworm-slim
COPY --from=build /src/target/release/agent-mcp-example /usr/local/bin/mcp
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/mcp"]
