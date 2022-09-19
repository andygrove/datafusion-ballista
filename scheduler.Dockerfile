FROM ubuntu:22.04

COPY ./target/release/ballista-scheduler .

EXPOSE 50050

CMD ./ballista-scheduler
