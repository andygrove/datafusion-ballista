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

FROM rust:1.63.0-buster as rust-build

ARG RELEASE_FLAG=--release

ENV RELEASE_FLAG=${RELEASE_FLAG}
ENV RUST_LOG=info
ENV RUST_BACKTRACE=full
ENV FORCE_REBUILD='true'

RUN apt-get update && \
    apt-get -y install libssl-dev openssl zlib1g zlib1g-dev libpq-dev cmake protobuf-compiler netcat nginx && \
    rm -rf /var/www/html/* && \
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
RUN cargo build --features flight-sql --manifest-path ballista/rust/scheduler/Cargo.toml $RELEASE_FLAG && \
    mv target/**/ballista-scheduler /scheduler && \
    rm -rf /tmp/*

# Use node image to build the scheduler UI
FROM node:14.16.0-alpine as ui-build
WORKDIR /app
ENV PATH /app/node_modules/.bin:$PATH
COPY ballista/ui/scheduler ./
RUN yarn install && yarn build

FROM rust-build
COPY --from=ui-build /app/build /var/www/html

# Expose Ballista Scheduler web UI port
EXPOSE 80

# Expose Ballista Scheduler gRPC port
EXPOSE 50050

ADD dev/docker/scheduler-entrypoint.sh /
ENTRYPOINT ["/scheduler-entrypoint.sh"]
