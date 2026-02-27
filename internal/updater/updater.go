// Package updater orchestrates the container update cycle.
package updater

import (
	"bufio"
	"context"
	"fmt"
	"log/slog"
	"os"
	"regexp"
	"strings"

	"github.com/docker/docker/client"

	"github.com/dirdmaster/isengard/internal/config"
	"github.com/dirdmaster/isengard/internal/container"
	"github.com/dirdmaster/isengard/internal/docker"
	"github.com/dirdmaster/isengard/internal/registry"
)

const labelEnable = "isengard.enable"

// Updater watches running containers for newer images and recreates them
// in-place, preserving ports, volumes, networks, labels, and restart policies.
type Updater struct {
	cli    *client.Client
	config config.Config
	selfID string
}

// New configures an [Updater] and detects whether it is running inside
// a container so it can exclude itself from update checks.
func New(cli *client.Client, cfg config.Config) *Updater {
	return &Updater{
		cli:    cli,
		config: cfg,
		selfID: detectSelfID(),
	}
}

// RunCycle performs a single update check cycle using a hybrid approach:
//  1. For each candidate container, check the remote registry digest via HEAD request (~50ms)
//  2. Compare against the local image's RepoDigests
//  3. Only pull the image if the digest differs (or if the HEAD check fails as fallback)
//  4. Recreate containers that have a newer image available
//
// Returns the number of containers updated and any error.
func (u *Updater) RunCycle(ctx context.Context) (int, error) {
	containers, err := container.ListRunning(ctx, u.cli)
	if err != nil {
		return 0, fmt.Errorf("listing containers: %w", err)
	}

	slog.Info("starting update cycle", "containers_found", len(containers))

	// Filter — separate self from other candidates
	var candidates []container.Info
	var selfContainer *container.Info
	for _, c := range containers {
		if u.isSelf(c.ID) {
			if u.config.SelfUpdate {
				cc := c // copy for pointer stability
				selfContainer = &cc
				slog.Debug("found self, deferring update check", "container", c.Name)
			} else {
				slog.Debug("skipping self", "container", c.Name)
			}
			continue
		}
		if u.shouldSkip(c) {
			continue
		}
		candidates = append(candidates, c)
	}

	slog.Info("checking for updates", "candidates", len(candidates))

	// Check each candidate using hybrid digest approach
	var toUpdate []container.Info
	oldImageIDs := map[string]string{}

	for _, c := range candidates {
		needsUpdate, err := u.checkForUpdate(ctx, c)
		if err != nil {
			slog.Warn("update check failed", "container", c.Name, "image", c.Image, "error", err)
			continue
		}

		if needsUpdate {
			oldImageIDs[c.ID] = c.ImageID
			toUpdate = append(toUpdate, c)
		}
	}

	// Update containers that have newer images
	updated := 0
	if len(toUpdate) > 0 {
		slog.Info("updating containers", "count", len(toUpdate))

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
	} else {
		slog.Info("all containers up to date")
	}

	// Self-update runs last, after all other containers are handled.
	// This calls Recreate on our own container, which will kill this process.
	// The new container starts from the updated image and takes over.
	if selfContainer != nil {
		if err := u.trySelfUpdate(ctx, *selfContainer); err != nil {
			slog.Error("self-update failed", "error", err)
		}
		// If trySelfUpdate succeeded, we won't reach here — the process is dead.
	}

	return updated, nil
}

// trySelfUpdate checks if Isengard's own container has a newer image and
// recreates it if so. This is the last operation in a cycle because Recreate
// will stop and remove our own container, killing this process. The new
// container starts from the updated image.
func (u *Updater) trySelfUpdate(ctx context.Context, self container.Info) error {
	// Docker's container list API may resolve Image to a sha256 ref when the
	// local tag has been updated (e.g. a newer image was pulled or built with
	// the same tag). Inspect the container to recover the original image
	// reference stored in Config.Image, which preserves the tag.
	if strings.HasPrefix(self.Image, "sha256:") {
		inspect, err := u.cli.ContainerInspect(ctx, self.ID)
		if err == nil && !strings.HasPrefix(inspect.Config.Image, "sha256:") {
			slog.Debug("self-update: recovered image ref from config",
				"was", self.Image[:19],
				"now", inspect.Config.Image,
			)
			self.Image = inspect.Config.Image
		}
	}

	needsUpdate, err := u.checkForUpdate(ctx, self)
	if err != nil {
		return fmt.Errorf("checking self for update: %w", err)
	}

	if !needsUpdate {
		slog.Debug("self is up to date", "image", self.Image)
		return nil
	}

	slog.Info("self-update available, recreating isengard",
		"container", self.Name,
		"image", self.Image,
	)

	// Use an independent context. RecreateSelf will rename us, create and
	// start the replacement, then force-remove our container (killing this
	// process). The replacement must already be running before we die.
	recreateCtx := context.Background()

	_, err = container.RecreateSelf(recreateCtx, u.cli, self.ID, self.Image, u.config.StopTimeout)
	if err != nil {
		return fmt.Errorf("self-update: %w", err)
	}

	// If we reach here, something unexpected happened — RecreateSelf should
	// have killed this process by force-removing our container.
	slog.Warn("self-update: process still running after recreate")
	return nil
}

// checkForUpdate determines whether a container has a newer image available.
// It first tries the fast registry digest check, and falls back to pull-and-compare
// if the digest check fails.
func (u *Updater) checkForUpdate(ctx context.Context, c container.Info) (bool, error) {
	// Try fast digest check first
	slog.Debug("checking digest", "container", c.Name, "image", c.Image)

	remoteDigest, err := registry.CheckDigest(c.Image)
	if err != nil {
		// Digest check failed — fall back to pull-and-compare
		slog.Debug("digest check failed, falling back to pull",
			"container", c.Name,
			"image", c.Image,
			"error", err,
		)
		return u.pullAndCompare(ctx, c)
	}

	// Compare remote digest against local RepoDigests
	localDigest := extractLocalDigest(c)
	if localDigest == "" {
		// No local digest available — must pull to check
		slog.Debug("no local digest available, falling back to pull",
			"container", c.Name,
			"image", c.Image,
		)
		return u.pullAndCompare(ctx, c)
	}

	if remoteDigest == localDigest {
		slog.Debug("image up to date (digest match)",
			"container", c.Name,
			"image", c.Image,
			"digest", remoteDigest[:19],
		)
		return false, nil
	}

	// Digest differs — pull the new image so it's available for recreate
	slog.Info("update available (digest mismatch)",
		"container", c.Name,
		"image", c.Image,
		"local", localDigest[:19],
		"remote", remoteDigest[:19],
	)

	_, err = docker.PullImage(ctx, u.cli, c.Image)
	if err != nil {
		return false, fmt.Errorf("pulling updated image: %w", err)
	}

	return true, nil
}

// pullAndCompare is the fallback method: pull the image and compare image IDs.
func (u *Updater) pullAndCompare(ctx context.Context, c container.Info) (bool, error) {
	slog.Debug("pulling image", "container", c.Name, "image", c.Image)

	newImageID, err := docker.PullImage(ctx, u.cli, c.Image)
	if err != nil {
		return false, fmt.Errorf("pulling image: %w", err)
	}

	if newImageID != c.ImageID {
		slog.Info("update available (pull comparison)",
			"container", c.Name,
			"image", c.Image,
			"old_id", c.ImageID[:12],
			"new_id", newImageID[:12],
		)
		return true, nil
	}

	slog.Debug("image up to date (pull comparison)", "container", c.Name, "image", c.Image)
	return false, nil
}

// extractLocalDigest extracts the digest from a container's RepoDigests.
// RepoDigests entries look like "nginx@sha256:abc123..." or
// "docker.io/library/nginx@sha256:abc123...".
// We return just the "sha256:..." part to compare against the registry's
// Docker-Content-Digest header.
func extractLocalDigest(c container.Info) string {
	if len(c.RepoDigests) == 0 {
		return ""
	}

	// Parse the image ref to match against the right RepoDigest entry
	ref := registry.ParseImageRef(c.Image)
	fullRepo := ref.Registry + "/" + ref.Repository

	for _, rd := range c.RepoDigests {
		if i := strings.LastIndex(rd, "@"); i >= 0 {
			repoName := rd[:i]
			digest := rd[i+1:]

			// Try exact match first
			if repoName == fullRepo || repoName == ref.Repository {
				return digest
			}

			// Docker Hub images might be stored as "docker.io/library/nginx"
			// or just "library/nginx" or "nginx"
			if ref.Registry == "registry-1.docker.io" {
				hubVariants := []string{
					"docker.io/" + ref.Repository,
					ref.Repository,
				}
				// For library images, also match the short name
				if strings.HasPrefix(ref.Repository, "library/") {
					shortName := strings.TrimPrefix(ref.Repository, "library/")
					hubVariants = append(hubVariants, shortName)
				}
				for _, v := range hubVariants {
					if repoName == v {
						return digest
					}
				}
			}
		}
	}

	// If we couldn't match by repo name, just return the first digest
	if i := strings.LastIndex(c.RepoDigests[0], "@"); i >= 0 {
		return c.RepoDigests[0][i+1:]
	}

	return ""
}

// shouldSkip returns true if a container should be excluded from updates.
//
// When WatchAll is true (default): all containers are watched unless labeled
// isengard.enable=false. When WatchAll is false (opt-in mode): only containers
// labeled isengard.enable=true are watched.
func (u *Updater) shouldSkip(c container.Info) bool {
	// Skip self — detectSelfID may return a 12-char hostname (short ID)
	// while c.ID is the full 64-char container ID, so check prefix too.
	if u.isSelf(c.ID) {
		slog.Debug("skipping self", "container", c.Name)
		return true
	}

	// Skip containers with no pullable image ref
	if c.Image == "" || strings.HasPrefix(c.Image, "sha256:") {
		slog.Debug("skipping container with no pullable image", "container", c.Name, "image", c.Image)
		return true
	}

	val, hasLabel := c.Labels[labelEnable]

	if u.config.WatchAll {
		// Watch-all mode (default): skip only if explicitly disabled
		if hasLabel && strings.EqualFold(val, "false") {
			slog.Debug("skipping disabled container", "container", c.Name)
			return true
		}
		return false
	}

	// Opt-in mode: skip unless explicitly enabled
	if hasLabel && strings.EqualFold(val, "true") {
		return false
	}
	slog.Debug("skipping container (opt-in mode, not enabled)", "container", c.Name)
	return true
}

// isSelf returns true if the given container ID matches Isengard's own container.
// Handles both exact matches (full 64-char ID) and prefix matches (12-char hostname).
func (u *Updater) isSelf(containerID string) bool {
	if u.selfID == "" {
		return false
	}
	return containerID == u.selfID || strings.HasPrefix(containerID, u.selfID)
}

// containerIDPattern matches a 64-character lowercase hex string (Docker container ID).
var containerIDPattern = regexp.MustCompile(`[a-f0-9]{64}`)

// detectSelfID tries to detect our own container ID.
// It tries three methods in order, returning the first successful result.
func detectSelfID() string {
	// Method 1: hostname is often the short container ID.
	// Docker sets it to the first 12 hex chars of the container ID by default,
	// but docker compose overrides it to the service name, so we validate
	// that the hostname is actually a hex string.
	hostname, err := os.Hostname()
	if err == nil && len(hostname) == 12 && isHex(hostname) {
		return hostname
	}

	// Method 2: /proc/1/cpuset contains /docker/<id> on cgroup v1.
	data, err := os.ReadFile("/proc/1/cpuset")
	if err == nil {
		line := strings.TrimSpace(string(data))
		if strings.HasPrefix(line, "/docker/") {
			return strings.TrimPrefix(line, "/docker/")
		}
	}

	// Method 3: /proc/self/mountinfo contains the container ID in mount paths.
	// Docker mounts per-container files (hostname, resolv.conf, etc.) from
	// /var/lib/docker/containers/<id>/. This works on both cgroup v1 and v2.
	if id := parseMountinfo("/proc/self/mountinfo"); id != "" {
		return id
	}

	return ""
}

// isHex returns true if s contains only lowercase hexadecimal characters.
func isHex(s string) bool {
	for _, c := range s {
		if !((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f')) {
			return false
		}
	}
	return len(s) > 0
}

// parseMountinfo reads a mountinfo file and extracts a Docker container ID
// from mount source paths like /var/lib/docker/containers/<64-char-hex>/hostname.
func parseMountinfo(path string) string {
	f, err := os.Open(path)
	if err != nil {
		return ""
	}
	defer f.Close()

	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		line := scanner.Text()
		if !strings.Contains(line, "/docker/containers/") {
			continue
		}
		if match := containerIDPattern.FindString(line); match != "" {
			return match
		}
	}
	return ""
}
