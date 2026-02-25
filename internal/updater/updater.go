// Package updater orchestrates the container update cycle.
package updater

import (
	"context"
	"fmt"
	"log/slog"
	"os"
	"strings"

	"github.com/docker/docker/client"

	"github.com/docker-watcher/isengard/internal/config"
	"github.com/docker-watcher/isengard/internal/container"
	"github.com/docker-watcher/isengard/internal/docker"
)

const labelEnable = "isengard.enable"

// Updater orchestrates the container update cycle.
type Updater struct {
	cli    *client.Client
	config config.Config
	selfID string
}

// New creates a new Updater instance.
func New(cli *client.Client, cfg config.Config) *Updater {
	return &Updater{
		cli:    cli,
		config: cfg,
		selfID: detectSelfID(),
	}
}

// RunCycle performs a single update check cycle.
// Returns the number of containers updated and any error.
func (u *Updater) RunCycle(ctx context.Context) (int, error) {
	containers, err := container.ListRunning(ctx, u.cli)
	if err != nil {
		return 0, fmt.Errorf("listing containers: %w", err)
	}

	slog.Info("starting update cycle", "containers_found", len(containers))

	// Filter
	var candidates []container.Info
	for _, c := range containers {
		if u.shouldSkip(c) {
			continue
		}
		candidates = append(candidates, c)
	}

	slog.Info("checking for updates", "candidates", len(candidates))

	// Check each candidate
	var toUpdate []container.Info
	oldImageIDs := map[string]string{}

	for _, c := range candidates {
		slog.Debug("pulling image", "container", c.Name, "image", c.Image)

		newImageID, err := docker.PullImage(ctx, u.cli, c.Image)
		if err != nil {
			slog.Warn("failed to pull image", "container", c.Name, "image", c.Image, "error", err)
			continue
		}

		if newImageID != c.ImageID {
			slog.Info("update available",
				"container", c.Name,
				"image", c.Image,
				"old_id", c.ImageID[:12],
				"new_id", newImageID[:12],
			)
			oldImageIDs[c.ID] = c.ImageID
			toUpdate = append(toUpdate, c)
		} else {
			slog.Debug("image up to date", "container", c.Name, "image", c.Image)
		}
	}

	if len(toUpdate) == 0 {
		slog.Info("all containers up to date")
		return 0, nil
	}

	slog.Info("updating containers", "count", len(toUpdate))

	// Update sequentially
	updated := 0
	for _, c := range toUpdate {
		slog.Info("updating container", "container", c.Name, "image", c.Image)

		newID, err := container.Recreate(ctx, u.cli, c.ID, c.Image, u.config.StopTimeout)
		if err != nil {
			slog.Error("failed to update container", "container", c.Name, "error", err)
			continue
		}

		slog.Info("container updated",
			"container", c.Name,
			"old_id", c.ID[:12],
			"new_id", newID[:12],
		)
		updated++

		if u.config.Cleanup {
			if oldID, ok := oldImageIDs[c.ID]; ok {
				docker.RemoveImage(ctx, u.cli, oldID)
			}
		}
	}

	slog.Info("update cycle complete",
		"checked", len(candidates),
		"updated", updated,
		"failed", len(toUpdate)-updated,
	)

	return updated, nil
}

// shouldSkip returns true if a container should be excluded from updates.
func (u *Updater) shouldSkip(c container.Info) bool {
	// Skip self
	if c.ID == u.selfID {
		slog.Debug("skipping self", "container", c.Name)
		return true
	}

	// Check opt-out label
	if val, ok := c.Labels[labelEnable]; ok {
		if strings.EqualFold(val, "false") {
			slog.Debug("skipping disabled container", "container", c.Name)
			return true
		}
	}

	// Skip containers with no pullable image ref
	if c.Image == "" || strings.HasPrefix(c.Image, "sha256:") {
		slog.Debug("skipping container with no pullable image", "container", c.Name, "image", c.Image)
		return true
	}

	return false
}

// detectSelfID tries to detect our own container ID.
func detectSelfID() string {
	// Method 1: hostname is often the short container ID
	hostname, err := os.Hostname()
	if err == nil && len(hostname) == 12 {
		return hostname
	}

	// Method 2: /proc/1/cpuset (Docker sets this to /docker/<id>)
	data, err := os.ReadFile("/proc/1/cpuset")
	if err == nil {
		line := strings.TrimSpace(string(data))
		if strings.HasPrefix(line, "/docker/") {
			return strings.TrimPrefix(line, "/docker/")
		}
	}

	return ""
}
