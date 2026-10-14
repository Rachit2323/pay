# Build stage: compile the mock PSP.
FROM rust:1.97 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

# Run stage: only the compiled binary.
FROM debian:bookworm-slim
COPY --from=builder /app/target/release/mock_psp /usr/local/bin/mock_psp
EXPOSE 3001
CMD ["mock_psp"]
