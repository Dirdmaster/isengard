FROM golang:1.25-alpine AS builder

RUN apk add --no-cache ca-certificates upx

WORKDIR /build

COPY go.mod go.sum ./
RUN go mod download

COPY . .
RUN CGO_ENABLED=0 go build -ldflags="-s -w" -o /isengard .
RUN upx --best /isengard

FROM scratch

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /isengard /isengard

ENTRYPOINT ["/isengard"]
