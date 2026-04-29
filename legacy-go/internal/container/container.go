// Package container provides Docker container inspection, listing, and recreation.
package container

import (
	"context"
	"fmt"
	"log/slog"
	"strings"

	containertypes "github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/mount"
	"github.com/docker/docker/api/types/network"
	"github.com/docker/docker/client"
)

// Info captures the subset of container state needed for update checks
// and faithful recreation.
type Info struct {
	ID      string            // Docker container ID.
	Name    string            // Container name without leading slash.
	Image   string            // Image reference as specified at creation (e.g. "nginx:1.25").
	ImageID string            // Resolved image ID (sha256:...).
	Labels  map[string]string // Container labels, used for isengard.enable filtering.
	State   string            // Docker state string (running, exited, etc.).
	// RepoDigests from the image inspect, e.g. ["nginx@sha256:abc..."].
	// Used for fast digest comparison against the remote registry.
	RepoDigests []string
}

// ListRunning returns all running containers, enriching each with
// RepoDigests from an image inspect call for digest-based update checks.
func ListRunning(ctx context.Context, cli *client.Client) ([]Info, error) {
	containers, err := cli.ContainerList(ctx, containertypes.ListOptions{All: false})
	if err != nil {
		return nil, fmt.Errorf("listing containers: %w", err)
	}

	var result []Info
	for _, c := range containers {
		name := ""
		if len(c.Names) > 0 {
			name = c.Names[0]
			if name != "" && name[0] == '/' {
				name = name[1:]
			}
		}

		var repoDigests []string
		imgInspect, err := cli.ImageInspect(ctx, c.ImageID)
		if err == nil {
			repoDigests = imgInspect.RepoDigests
		}

		result = append(result, Info{
			ID:          c.ID,
			Name:        name,
			Image:       c.Image,
			ImageID:     c.ImageID,
			Labels:      c.Labels,
			State:       c.State,
			RepoDigests: repoDigests,
		})
	}

	return result, nil
}

// Recreate stops, removes, and recreates a container with the same config
// but a new image. Returns the new container ID.
func Recreate(ctx context.Context, cli *client.Client, containerID, newImageID string, stopTimeout int) (string, error) {
	inspect, err := cli.ContainerInspect(ctx, containerID)
	if err != nil {
		return "", fmt.Errorf("inspecting container: %w", err)
	}

	containerName := inspect.Name
	if containerName != "" && containerName[0] == '/' {
		containerName = containerName[1:]
	}

	slog.Debug("captured container config",
		"container", containerName,
		"old_image", inspect.Config.Image,
	)

	// Stop
	timeout := stopTimeout
	stopOpts := containertypes.StopOptions{Timeout: &timeout}
	if err := cli.ContainerStop(ctx, containerID, stopOpts); err != nil {
		slog.Warn("error stopping container, forcing remove", "container", containerName, "error", err)
	}

	// Remove
	if err := cli.ContainerRemove(ctx, containerID, containertypes.RemoveOptions{Force: true}); err != nil {
		return "", fmt.Errorf("removing container %s: %w", containerName, err)
	}

	// Build new config
	config := inspect.Config
	config.Image = newImageID

	hostConfig := inspect.HostConfig

	// Convert mounts back to proper mount configuration
	if len(inspect.Mounts) > 0 && len(hostConfig.Mounts) == 0 {
		hostConfig.Mounts = convertMounts(inspect.Mounts)
	}

	// Remove mounts/volumes that overlap with binds to prevent Docker from
	// rejecting the create with "Duplicate mount point".
	deduplicateMounts(hostConfig)
	deduplicateVolumes(config, hostConfig)

	// Prepare networking — connect to the first network during create
	var networkingConfig *network.NetworkingConfig
	additionalNetworks := map[string]*network.EndpointSettings{}

	if inspect.NetworkSettings != nil && len(inspect.NetworkSettings.Networks) > 0 {
		first := true
		for netName, netSettings := range inspect.NetworkSettings.Networks {
			epSettings := &network.EndpointSettings{
				Aliases: netSettings.Aliases,
			}
			if netSettings.IPAMConfig != nil {
				epSettings.IPAMConfig = &network.EndpointIPAMConfig{
					IPv4Address: netSettings.IPAMConfig.IPv4Address,
				}
			}
			if first {
				networkingConfig = &network.NetworkingConfig{
					EndpointsConfig: map[string]*network.EndpointSettings{
						netName: epSettings,
					},
				}
				first = false
			} else {
				additionalNetworks[netName] = epSettings
			}
		}
	}

	// Create
	createResp, err := cli.ContainerCreate(ctx, config, hostConfig, networkingConfig, nil, containerName)
	if err != nil {
		return "", fmt.Errorf("creating container %s: %w", containerName, err)
	}

	// Connect additional networks
	for netName, epSettings := range additionalNetworks {
		if err := cli.NetworkConnect(ctx, netName, createResp.ID, epSettings); err != nil {
			slog.Warn("failed to connect network", "container", containerName, "network", netName, "error", err)
		}
	}

	// Start
	if err := cli.ContainerStart(ctx, createResp.ID, containertypes.StartOptions{}); err != nil {
		return "", fmt.Errorf("starting container %s: %w", containerName, err)
	}

	return createResp.ID, nil
}

// RecreateSelf recreates Isengard's own container with a safe ordering that
// ensures the replacement is running before the old container is killed.
//
// Unlike [Recreate], which does stop -> remove -> create -> start, this
// function does: inspect -> rename self -> create replacement -> start
// replacement -> force-remove self. This prevents the race where stopping
// our own container kills the process before the replacement is created.
func RecreateSelf(ctx context.Context, cli *client.Client, containerID, newImage string, _ int) (string, error) {
	inspect, err := cli.ContainerInspect(ctx, containerID)
	if err != nil {
		return "", fmt.Errorf("inspecting container: %w", err)
	}

	containerName := inspect.Name
	if containerName != "" && containerName[0] == '/' {
		containerName = containerName[1:]
	}

	slog.Debug("self-update: captured container config",
		"container", containerName,
		"old_image", inspect.Config.Image,
	)

	// Build new config with updated image
	config := inspect.Config
	config.Image = newImage

	hostConfig := inspect.HostConfig

	if len(inspect.Mounts) > 0 && len(hostConfig.Mounts) == 0 {
		hostConfig.Mounts = convertMounts(inspect.Mounts)
	}
	deduplicateMounts(hostConfig)
	deduplicateVolumes(config, hostConfig)

	// Prepare networking
	var networkingConfig *network.NetworkingConfig
	additionalNetworks := map[string]*network.EndpointSettings{}

	if inspect.NetworkSettings != nil && len(inspect.NetworkSettings.Networks) > 0 {
		first := true
		for netName, netSettings := range inspect.NetworkSettings.Networks {
			epSettings := &network.EndpointSettings{
				Aliases: netSettings.Aliases,
			}
			if netSettings.IPAMConfig != nil {
				epSettings.IPAMConfig = &network.EndpointIPAMConfig{
					IPv4Address: netSettings.IPAMConfig.IPv4Address,
				}
			}
			if first {
				networkingConfig = &network.NetworkingConfig{
					EndpointsConfig: map[string]*network.EndpointSettings{
						netName: epSettings,
					},
				}
				first = false
			} else {
				additionalNetworks[netName] = epSettings
			}
		}
	}

	// Rename self to free up the container name for the replacement.
	tempName := containerName + "-old"
	slog.Debug("self-update: renaming self", "from", containerName, "to", tempName)
	if err := cli.ContainerRename(ctx, containerID, tempName); err != nil {
		return "", fmt.Errorf("renaming self: %w", err)
	}

	// Create replacement with the original name
	slog.Debug("self-update: creating replacement", "container", containerName, "image", newImage)
	createResp, err := cli.ContainerCreate(ctx, config, hostConfig, networkingConfig, nil, containerName)
	if err != nil {
		// Try to restore original name if create fails
		_ = cli.ContainerRename(ctx, containerID, containerName)
		return "", fmt.Errorf("creating replacement: %w", err)
	}

	// Connect additional networks
	for netName, epSettings := range additionalNetworks {
		if err := cli.NetworkConnect(ctx, netName, createResp.ID, epSettings); err != nil {
			slog.Warn("failed to connect network", "container", containerName, "network", netName, "error", err)
		}
	}

	// Start replacement
	slog.Info("self-update: starting replacement", "container", containerName, "new_id", createResp.ID[:12])
	if err := cli.ContainerStart(ctx, createResp.ID, containertypes.StartOptions{}); err != nil {
		return "", fmt.Errorf("starting replacement: %w", err)
	}

	// Replacement is running. Force-remove ourselves. This sends SIGKILL and
	// our process dies immediately, but that's fine because the new container
	// is already running.
	slog.Info("self-update: replacement started, removing old container")
	_ = cli.ContainerRemove(ctx, containerID, containertypes.RemoveOptions{Force: true})

	// If we reach here, something unexpected happened.
	return createResp.ID, nil
}

// deduplicateVolumes removes entries from config.Volumes whose paths are
// already covered by an explicit mount in hostConfig.Mounts or hostConfig.Binds.
// This prevents "Duplicate mount point" errors when an image declares VOLUME
// directives that overlap with user-specified mounts.
func deduplicateVolumes(config *containertypes.Config, hostConfig *containertypes.HostConfig) {
	if config.Volumes == nil {
		return
	}

	covered := make(map[string]bool, len(hostConfig.Mounts)+len(hostConfig.Binds))

	for _, m := range hostConfig.Mounts {
		covered[m.Target] = true
	}

	// Binds use "source:dest" or "source:dest:opts" format.
	for _, b := range hostConfig.Binds {
		parts := strings.SplitN(b, ":", 3)
		if len(parts) >= 2 {
			covered[parts[1]] = true
		}
	}

	for path := range config.Volumes {
		if covered[path] {
			delete(config.Volumes, path)
		}
	}
}

// deduplicateMounts removes entries from hostConfig.Mounts whose target paths
// are already covered by hostConfig.Binds. When Docker inspects a container,
// bind mounts may appear in both HostConfig.Binds and the top-level Mounts
// array. If convertMounts then populates HostConfig.Mounts from the inspect
// Mounts, the same path ends up in both Binds and Mounts, causing Docker to
// reject the create with "Duplicate mount point".
func deduplicateMounts(hostConfig *containertypes.HostConfig) {
	if len(hostConfig.Mounts) == 0 || len(hostConfig.Binds) == 0 {
		return
	}

	bindTargets := make(map[string]bool, len(hostConfig.Binds))
	for _, b := range hostConfig.Binds {
		parts := strings.SplitN(b, ":", 3)
		if len(parts) >= 2 {
			bindTargets[parts[1]] = true
		}
	}

	filtered := hostConfig.Mounts[:0]
	for _, m := range hostConfig.Mounts {
		if !bindTargets[m.Target] {
			filtered = append(filtered, m)
		}
	}
	hostConfig.Mounts = filtered
}

// convertMounts converts docker inspect mount points back to mount.Mount format.
func convertMounts(mountPoints []containertypes.MountPoint) []mount.Mount {
	var mounts []mount.Mount
	for _, mp := range mountPoints {
		m := mount.Mount{
			Type:     mount.Type(mp.Type),
			Source:   mp.Source,
			Target:   mp.Destination,
			ReadOnly: !mp.RW,
		}
		mounts = append(mounts, m)
	}
	return mounts
}
