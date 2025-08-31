FROM rust:1.88-bookworm AS builder
WORKDIR /usr/src/app

# We currently use protoc rather than relying on the protobuf-sys package
# because it greatly cuts down on build times. This may change in the future.
RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

# Copy over only the files which specify dependencies
COPY ./Cargo.toml ./Cargo.lock ./

# We need to create a dummy main in order to get this to properly build.
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release

# Copy over the files to actually build the application.
COPY . .

# We need to make sure the update time on main.rs is newer than the temporary
# file or there are weird cargo caching issues we run into.
RUN touch src/main.rs && cargo build --release && cp -v target/release/seabird-radio /usr/local/bin

# Create a new base and copy in only what we need.
FROM debian:bookworm-slim
ENV RUST_LOG=info
ENV SEABIRD_TOKEN=fill_me_in

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/app

COPY --from=builder /usr/local/bin/seabird-radio /usr/local/bin/
CMD ["/usr/local/bin/seabird-radio"]
