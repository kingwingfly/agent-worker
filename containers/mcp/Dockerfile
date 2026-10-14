# The shared MCP server's container. One port is open, 8080, and behind it sits
# `mcp-front`: it starts the MCP server, serves the files that server saves, and sends
# each request to whichever of the two it is for.
#
# This builds the placeholder MCP server from crates/mcp-example. To run your own, put its
# binary in the image and name it as the command after `mcp-front` at the bottom. It has
# to serve MCP over streamable HTTP at /mcp on the address in MCP_FRONT_MCP_ADDR
# (127.0.0.1:8081 unless you set it), and save any file an agent should be able to fetch
# under MCP_FRONT_FILES_DIR. The build context is the repository root.
#
# Copy no credential in here: whoever can pull the image would have it. Make it a Worker
# secret and name it in MCP_CONTAINER_ENV (wrangler.toml), and it arrives in the server's
# environment when the container starts.

FROM rust:1-bookworm AS build
# pingora builds its zlib with cmake.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p agent-mcp-front -p agent-mcp-example

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 1000 mcp
COPY --from=build /src/target/release/mcp-front /usr/local/bin/mcp-front
COPY --from=build /src/target/release/agent-mcp-example /usr/local/bin/mcp

USER mcp
# The directory is made by mcp-front when it starts, so it only has to be creatable.
ENV HOME=/home/mcp \
    MCP_FRONT_FILES_DIR=/home/mcp/files
EXPOSE 8080
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/mcp-front", "/usr/local/bin/mcp"]
