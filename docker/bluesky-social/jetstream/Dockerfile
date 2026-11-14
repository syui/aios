ARG AIOS=ghcr.io/syui/aios:0.0.1.r987.g13104fa@sha256:3f12715a3d0fe2baf7e796e88f4792852d2c9328025304b0438df1aa42de9dd0
ARG AIOS_STATIC=ghcr.io/syui/aios-static:20261105@sha256:6039d9e67630100421bcb1b1b0e769df59fe251ce34237b07896e3da9e878231

FROM ${AIOS} AS build
RUN ap -Sy --noconfirm go
ARG VERSION=dev
ARG COMMIT=unknown
ARG DATE=unknown
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -buildvcs=false -tags timetzdata \
    -ldflags "-X github.com/bluesky-social/jetstream/internal/version.Version=${VERSION} -X github.com/bluesky-social/jetstream/internal/version.Commit=${COMMIT} -X github.com/bluesky-social/jetstream/internal/version.Date=${DATE}" \
    -o /out/jetstream ./cmd/jetstream

FROM ${AIOS_STATIC}
COPY --from=build /out/jetstream /usr/local/bin/jetstream
EXPOSE 8080 6060
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/jetstream"]
CMD ["serve"]
