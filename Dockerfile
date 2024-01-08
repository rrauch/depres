FROM debian:12 as builder

RUN apt-get update && apt-get -y upgrade \
 && apt-get -y install wget curl build-essential gcc make libssl-dev pkg-config fuse

RUN curl https://sh.rustup.rs -sSf | bash -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
ENV RUSTFLAGS="-C target-feature=+crt-static"

COPY ./Cargo.* /usr/local/src/depres/

RUN cd /usr/local/src/depres/ \
 && mkdir src \
 && echo "// dummy file" > src/lib.rs \
 && cargo build --release

COPY ./src /usr/local/src/depres/src/

RUN cd /usr/local/src/depres/ \
 && cargo build --release \
 && cp ./target/release/depres /usr/local/bin/
 
FROM scratch
COPY --from=builder /usr/local/bin/depres /
