# Licensed to the Apache Software Foundation (ASF) under one
# or more contributor license agreements.  See the NOTICE file
# distributed with this work for additional information
# regarding copyright ownership.  The ASF licenses this file
# to you under the Apache License, Version 2.0 (the
# "License"); you may not use this file except in compliance
# with the License.  You may obtain a copy of the License at
#
#   http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing,
# software distributed under the License is distributed on an
# "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
# KIND, either express or implied.  See the License for the
# specific language governing permissions and limitations
# under the License.

FROM rust:1.63.0-buster

ARG RELEASE_FLAG=--release

ENV RELEASE_FLAG=${RELEASE_FLAG}
ENV RUST_LOG=info
ENV RUST_BACKTRACE=full
ENV FORCE_REBUILD='true'

RUN apt-get update && \
    apt-get -y install libssl-dev openssl zlib1g zlib1g-dev libpq-dev cmake protobuf-compiler netcat && \
    rm -rf /var/lib/apt/lists/*

# prepare toolchain
RUN rustup update && \
    rustup component add rustfmt && \
    cargo install cargo-chef --version 0.1.34

WORKDIR /tmp/ballista

ADD Cargo.toml .
COPY ballista/ ./ballista
COPY ballista-cli/ ./ballista-cli
COPY examples/ ./examples
COPY benchmarks/ ./benchmarks

# force build.rs to run to generate configure_me code.
RUN cargo build --manifest-path ballista/rust/executor/Cargo.toml $RELEASE_FLAG && \
  mv target/**/ballista-executor /executor && \
  rm -rf /tmp/*

# Expose Ballista Executor gRPC port
EXPOSE 50051

ADD dev/docker/executor-entrypoint.sh /
ENTRYPOINT ["/executor-entrypoint.sh"]
