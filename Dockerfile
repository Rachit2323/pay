# Build stage: compile the invoice service.
FROM rust:1.97 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

# Run stage: only the compiled binary.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libssl3 ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/dodo_payment /usr/local/bin/dodo_payment
EXPOSE 3000
CMD ["dodo_payment"]
