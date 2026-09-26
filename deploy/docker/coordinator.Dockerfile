# The coordinator image, assembled from the release's own binaries and WASM
# package (see .github/workflows/release.yml). Nothing compiles here, so an
# image takes seconds, and with no RUN step the arm64 image needs no emulation.
#
# Build context (made by the release workflow):
#   bin/<arch>/coordinator, bin/<arch>/wallet-cli   linux binaries per arch
#   ui/pkg/                                         the WASM package, with .br and .gz copies
FROM gcr.io/distroless/cc-debian13@sha256:4594d59540d1948417f6ca2829ddd9294493a7c68b7528f4dd459de7f203a750
ARG TARGETARCH
COPY bin/${TARGETARCH}/coordinator bin/${TARGETARCH}/wallet-cli /app/
COPY ui/ /app/ui/
ENV PATH=/app:/usr/local/bin:/usr/bin:/bin \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    RUST_LOG=info
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8080
CMD ["/app/coordinator"]
