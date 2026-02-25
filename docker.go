package main

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"io"
	"log/slog"
	"os"
	"strings"

	"github.com/docker/docker/api/types/image"
	"github.com/docker/docker/api/types/registry"
	"github.com/docker/docker/client"
)

// NewDockerClient creates a Docker client from environment defaults.
func NewDockerClient() (*client.Client, error) {
	return client.NewClientWithOpts(
		client.FromEnv,
		client.WithAPIVersionNegotiation(),
	)
}

// PullImage pulls the latest version of an image and returns the new image ID.
// It uses credentials from ~/.docker/config.json if available.
func PullImage(ctx context.Context, cli *client.Client, imageRef string) (string, error) {
	opts := image.PullOptions{}

	// Try to get auth for this registry
	auth := getAuthForImage(imageRef)
	if auth != "" {
		opts.RegistryAuth = auth
	}

	reader, err := cli.ImagePull(ctx, imageRef, opts)
	if err != nil {
		return "", err
	}
	defer reader.Close()

	// Consume the pull output (required to complete the pull)
	_, _ = io.Copy(io.Discard, reader)

	// Get the image ID after pull
	inspect, _, err := cli.ImageInspectWithRaw(ctx, imageRef)
	if err != nil {
		return "", err
	}

	return inspect.ID, nil
}

// GetImageID returns the image ID for a given image reference.
func GetImageID(ctx context.Context, cli *client.Client, imageRef string) (string, error) {
	inspect, _, err := cli.ImageInspectWithRaw(ctx, imageRef)
	if err != nil {
		return "", err
	}
	return inspect.ID, nil
}

// RemoveImage removes an image by ID, ignoring errors (image may be in use by other containers).
func RemoveImage(ctx context.Context, cli *client.Client, imageID string) {
	opts := image.RemoveOptions{PruneChildren: true}
	_, err := cli.ImageRemove(ctx, imageID, opts)
	if err != nil {
		slog.Debug("could not remove old image", "image", imageID[:12], "error", err)
	} else {
		slog.Info("removed old image", "image", imageID[:12])
	}
}

// dockerConfig represents the structure of ~/.docker/config.json
type dockerConfig struct {
	Auths map[string]dockerAuthEntry `json:"auths"`
}

type dockerAuthEntry struct {
	Auth string `json:"auth"`
}

// getAuthForImage reads ~/.docker/config.json and returns base64-encoded
// registry auth for the given image reference, or empty string if not found.
func getAuthForImage(imageRef string) string {
	configPath := "/root/.docker/config.json"
	if v := os.Getenv("DOCKER_CONFIG"); v != "" {
		configPath = v + "/config.json"
	}

	data, err := os.ReadFile(configPath)
	if err != nil {
		return ""
	}

	var cfg dockerConfig
	if err := json.Unmarshal(data, &cfg); err != nil {
		return ""
	}

	// Extract registry from image reference
	reg := extractRegistry(imageRef)

	// Try exact match first, then common variants
	candidates := []string{reg, "https://" + reg, "https://" + reg + "/v1/", "https://" + reg + "/v2/"}
	for _, candidate := range candidates {
		if entry, ok := cfg.Auths[candidate]; ok && entry.Auth != "" {
			authConfig := registry.AuthConfig{}
			decoded, err := base64.StdEncoding.DecodeString(entry.Auth)
			if err == nil {
				parts := strings.SplitN(string(decoded), ":", 2)
				if len(parts) == 2 {
					authConfig.Username = parts[0]
					authConfig.Password = parts[1]
				}
			}
			encoded, err := json.Marshal(authConfig)
			if err != nil {
				return ""
			}
			return base64.URLEncoding.EncodeToString(encoded)
		}
	}

	return ""
}

// extractRegistry returns the registry hostname from an image reference.
// "nginx" -> "docker.io"
// "ghcr.io/user/repo:tag" -> "ghcr.io"
// "registry.example.com:5000/image" -> "registry.example.com:5000"
func extractRegistry(imageRef string) string {
	// Remove tag/digest
	ref := imageRef
	if i := strings.LastIndex(ref, "@"); i >= 0 {
		ref = ref[:i]
	}
	if i := strings.LastIndex(ref, ":"); i >= 0 {
		// Only strip if after last slash (it's a tag, not a port)
		if j := strings.LastIndex(ref, "/"); j < i {
			ref = ref[:i]
		}
	}

	// If no slash, it's a Docker Hub library image
	if !strings.Contains(ref, "/") {
		return "docker.io"
	}

	parts := strings.SplitN(ref, "/", 2)
	first := parts[0]

	// Check if first part looks like a registry (has dots or colons, or is "localhost")
	if strings.Contains(first, ".") || strings.Contains(first, ":") || first == "localhost" {
		return first
	}

	// Otherwise it's a Docker Hub user image (e.g., "user/repo")
	return "docker.io"
}
