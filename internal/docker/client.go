// Package docker provides Docker client helpers for image operations.
package docker

import (
	"context"
	"io"
	"log/slog"

	"github.com/docker/docker/api/types/image"
	"github.com/docker/docker/client"

	"github.com/dirdmaster/isengard/internal/registry"
)

// NewClient connects to the local Docker daemon using DOCKER_HOST
// and related environment variables, with automatic API version negotiation.
func NewClient() (*client.Client, error) {
	return client.NewClientWithOpts(
		client.FromEnv,
		client.WithAPIVersionNegotiation(),
	)
}

// PullImage pulls the latest version of an image and returns the new image ID.
// It uses credentials from ~/.docker/config.json via the registry package.
func PullImage(ctx context.Context, cli *client.Client, imageRef string) (string, error) {
	opts := image.PullOptions{}

	if auth := registry.AuthForImage(imageRef); auth != "" {
		opts.RegistryAuth = auth
	}

	reader, err := cli.ImagePull(ctx, imageRef, opts)
	if err != nil {
		return "", err
	}
	defer reader.Close()

	// Consume the pull output (required to complete the pull)
	_, _ = io.Copy(io.Discard, reader)

	inspect, err := cli.ImageInspect(ctx, imageRef)
	if err != nil {
		return "", err
	}

	return inspect.ID, nil
}

// RemoveImage removes an image by ID, ignoring errors (image may be in use).
func RemoveImage(ctx context.Context, cli *client.Client, imageID string) {
	opts := image.RemoveOptions{PruneChildren: true}
	_, err := cli.ImageRemove(ctx, imageID, opts)
	if err != nil {
		slog.Debug("could not remove old image", "image", imageID[:12], "error", err)
	} else {
		slog.Info("removed old image", "image", imageID[:12])
	}
}
