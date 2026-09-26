# The synth image, assembled from the release's own binary; see
# coordinator.Dockerfile.
FROM gcr.io/distroless/cc-debian13@sha256:4594d59540d1948417f6ca2829ddd9294493a7c68b7528f4dd459de7f203a750
ARG TARGETARCH
COPY bin/${TARGETARCH}/synth /app/
ENV PATH=/app:/usr/local/bin:/usr/bin:/bin \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    RUST_LOG=info
WORKDIR /data
VOLUME ["/data"]
EXPOSE 9980
CMD ["/app/synth"]
